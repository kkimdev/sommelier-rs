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

use crate::virtwl_channel::{Transaction, VirtWaylandChannel};
use crate::wire::{Framing, MessageHeader, Outgoing};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use std::collections::VecDeque;
use std::io::{self, IoSlice};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use zerocopy::AsBytes;

// Linux accepts at most SCM_MAX_FD (253) descriptors in one SCM_RIGHTS
// message. Keeping the receive control buffer at that limit prevents a valid
// Unix peer from triggering MSG_CTRUNC merely because the proxy used a
// smaller, VirtWL-specific batch size. `decode_unix_control_fds` still
// closes the descriptors of truncated control data.
const UNIX_MAX_SCM_RIGHTS_FDS: usize = 253;

/// The room a receive of [`UNIX_MAX_SCM_RIGHTS_FDS`] descriptors needs.
// SAFETY: CMSG_SPACE only computes a length.
const UNIX_CONTROL_LEN: usize =
    unsafe { libc::CMSG_SPACE((UNIX_MAX_SCM_RIGHTS_FDS * std::mem::size_of::<RawFd>()) as u32) }
        as usize;

/// Room for one receive's control data, aligned for `cmsghdr` because the
/// CMSG macros read headers in place: the first `len` bytes of whole
/// `u64`s.
#[derive(Debug)]
struct ControlBuffer {
    words: Vec<u64>,
    len: usize,
}

// A `u64` is aligned for `cmsghdr` on every Linux target.
const _: () = assert!(std::mem::align_of::<libc::cmsghdr>() <= std::mem::align_of::<u64>());

impl ControlBuffer {
    fn new(len: usize) -> Self {
        let words = vec![0; len.div_ceil(std::mem::size_of::<u64>())];
        Self { words, len }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.words.as_bytes_mut()[..self.len]
    }
}

/// The most received descriptors a connection holds before messages
/// consume them.
///
/// Descriptors travel apart from the bytes, so a peer can attach them to
/// messages that take none and make the proxy hold them indefinitely; with
/// no bound, one client could exhaust the descriptor table that every
/// client shares. libwayland buffers a connection's incoming descriptors in
/// a 4096-byte ring, 1024 descriptors, and disconnects a peer that
/// overflows it. Using the same bound refuses no peer that a libwayland
/// compositor would accept.
const MAX_PENDING_FDS: usize = 1024;

/// The most descriptors one transport submission carries.
///
/// A VirtWL transaction has room for `VIRTWL_SEND_MAX_ALLOCS` (28), the
/// same number libwayland sends per `sendmsg` (`MAX_FDS_OUT`). Both
/// transports split batches at this bound, so the behavior tested over Unix
/// sockets is the behavior on the device.
pub(crate) const MAX_FDS_PER_SEND: usize = crate::virtwl::VIRTWL_SEND_MAX_ALLOCS;

/// The most bytes one transport submission carries, unless it is a single
/// longer message.
///
/// It is the data payload of a VirtWL transaction
/// (`virtwl_channel::MAX_SEND_SIZE`), which cuts a longer submission by
/// bytes. Exo's libwayland reads at most 4 KiB per dispatch, so a message
/// cut in two can be dispatched in two reads, apart from the messages
/// sent with it. Like [`MAX_FDS_PER_SEND`], both transports apply it.
pub(crate) const MAX_BYTES_PER_SEND: usize = crate::virtwl_channel::MAX_SEND_SIZE;

/// An ordered run of outgoing Wayland messages and their descriptors.
///
/// Sent by [`WaylandConnection::send_messages`], which coalesces messages
/// into as few submissions as [`MAX_FDS_PER_SEND`] and
/// [`MAX_BYTES_PER_SEND`] allow, or, while more may follow, only its full
/// submissions ([`LastSubmission::Keep`]). It owns the descriptors: a
/// send passes copies to the peer and then closes them, and dropping the
/// run closes the rest, sent or not.
///
/// A submission ends only at an entry of `ends`, one per pushed entry. An
/// entry may hold several messages, which a submission then never splits:
/// the IME glues a surrounding-text report or a `reset` and the
/// `wl_display.sync` after it into one (`ime::HostOut::Glued`).
///
/// Its messages are all for one peer, whose IDs `I` they carry
/// ([`Outgoing`]).
#[derive(Debug)]
pub(crate) struct OutgoingMessages<I> {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    /// Where each message ends in `bytes` and in `fds`.
    ends: Vec<(usize, usize)>,
    ids: PhantomData<I>,
}

impl<I> Default for OutgoingMessages<I> {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            fds: Vec::new(),
            ends: Vec::new(),
            ids: PhantomData,
        }
    }
}

impl<I> OutgoingMessages<I> {
    pub(crate) fn push(&mut self, message: Outgoing<I>) {
        let (message, fds) = message.into_parts();
        self.bytes.extend_from_slice(&message);
        self.fds.extend(fds);
        self.ends.push((self.bytes.len(), self.fds.len()));
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// How many entries wait to be sent.
    pub(crate) fn len(&self) -> usize {
        self.ends.len()
    }

    /// Each entry after the first `mark`, with how many descriptors it
    /// carries.
    pub(crate) fn since(&self, mark: usize) -> impl Iterator<Item = (&[u8], usize)> + '_ {
        let start = mark.checked_sub(1).map_or((0, 0), |last| self.ends[last]);
        self.ends[mark..].iter().scan(start, |start, &end| {
            let entry = (&self.bytes[start.0..end.0], end.1 - start.1);
            *start = end;
            Some(entry)
        })
    }

    /// Where each submission of these messages ends in `bytes` and in
    /// `fds`, in order: each takes as many whole entries as fit in
    /// [`MAX_BYTES_PER_SEND`] and [`MAX_FDS_PER_SEND`], or one entry that
    /// is longer on its own. An entry with more descriptors than a
    /// submission carries is an `InvalidInput` error, after which there is
    /// nothing more.
    fn submission_ends(&self) -> impl Iterator<Item = io::Result<(usize, usize)>> + '_ {
        let mut entries = self.ends.iter().copied().peekable();
        let mut start = (0, 0);
        std::iter::from_fn(move || {
            let mut end = start;
            while let Some(&entry) = entries.peek() {
                let fits = entry.0 - start.0 <= MAX_BYTES_PER_SEND
                    && entry.1 - start.1 <= MAX_FDS_PER_SEND;
                if !fits && end != start {
                    // This entry starts the next submission.
                    break;
                }
                let entry_fds = entry.1 - end.1;
                if entry_fds > MAX_FDS_PER_SEND {
                    entries.by_ref().for_each(drop);
                    return Some(Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("a message carries {entry_fds} descriptors"),
                    )));
                }
                entries.next();
                end = entry;
                if !fits {
                    // A message longer than a submission goes alone.
                    break;
                }
            }
            (end != start).then(|| {
                start = end;
                Ok(end)
            })
        })
    }

    /// Forget the messages that end at `sent` in `bytes` and `fds`, which
    /// closes their descriptors, and keep the capacity for the next ones.
    fn forget_sent(&mut self, sent: (usize, usize)) {
        self.bytes.drain(..sent.0);
        self.fds.drain(..sent.1);
        let sent_entries = self.ends.partition_point(|&(bytes, _)| bytes <= sent.0);
        self.ends.drain(..sent_entries);
        for end in &mut self.ends {
            end.0 -= sent.0;
            end.1 -= sent.1;
        }
    }

    /// Forget every message, which closes their descriptors, and keep the
    /// capacity for the next ones.
    fn clear(&mut self) {
        self.bytes.clear();
        self.fds.clear();
        self.ends.clear();
    }
}

/// Whether [`WaylandConnection::send_submissions`] sends the last
/// submission, which later messages could still fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LastSubmission {
    /// Send it: no message follows soon.
    Send,
    /// Keep it for the messages that may follow: send only the full
    /// submissions.
    Keep,
}

pub struct WaylandConnection {
    transport: ConnectionTransport,
    pub read_buf: Vec<u8>,
    /// Received descriptors no message has taken yet, in stream order. A
    /// decoded message takes its own from the front; the rest close with
    /// the connection.
    pub read_fds: VecDeque<OwnedFd>,
    /// A complete untracked message was dropped while descriptors remained
    /// queued on the ordered stream. This ambiguity must survive separate
    /// recv/parse calls until a complete follow-up can be rejected.
    pub ambiguous_untracked_fd: bool,
    /// The most received descriptors held unconsumed: [`MAX_PENDING_FDS`],
    /// which tests lower so they need not hold a thousand descriptors.
    max_pending_fds: usize,
}

/// A connection's transport, with the buffers it lends every receive and
/// send, so neither allocates one.
enum ConnectionTransport {
    Unix {
        socket: AsyncFd<OwnedFd>,
        /// Room for a receive's `SCM_RIGHTS` control data.
        control: ControlBuffer,
    },
    VirtWayland {
        channel: Arc<VirtWaylandChannel>,
        transaction: Transaction,
    },
}

fn unix_send_flags() -> MsgFlags {
    // A compositor or client can disappear while a message is queued. The
    // proxy must observe EPIPE and tear down that connection, not receive a
    // process-wide SIGPIPE and take unrelated GUI clients with it.
    MsgFlags::MSG_NOSIGNAL
}

/// Take over the descriptors of the `SCM_RIGHTS` messages in `control`, the
/// control data one `recvmsg` wrote, reporting `MSG_CTRUNC` as `truncated`.
///
/// `nix::RecvMsg::cmsgs()` refuses to iterate when `MSG_CTRUNC` is set,
/// which would leak the descriptors the kernel installed before it ran out
/// of room. The libc macros iterate truncated control data like any other,
/// so this takes over every descriptor present, skipping other messages,
/// before it reports truncated or malformed control data; returning the
/// error then closes them.
///
/// # Safety
///
/// `control` starts at an address aligned for `cmsghdr`, and the caller
/// owns every descriptor in its `SCM_RIGHTS` messages and passes them here.
// The kernel boundary: recvmsg installs raw descriptors.
#[allow(clippy::disallowed_methods)]
unsafe fn decode_unix_control_fds(control: &[u8], truncated: bool) -> io::Result<Vec<OwnedFd>> {
    debug_assert!(control.as_ptr().cast::<libc::cmsghdr>().is_aligned());
    // SAFETY: an all-zero `msghdr` is valid: null pointers, zero lengths.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_control = control.as_ptr().cast_mut().cast();
    // `size_t` on glibc, `socklen_t` on musl: a shorter length is still in
    // bounds.
    msg.msg_controllen = control.len() as _;
    let (mut fds, mut malformed, mut negative) = (Vec::new(), None, false);
    // SAFETY: `msg` describes `control`, which the caller aligned, so
    // CMSG_FIRSTHDR and CMSG_NXTHDR return null or an aligned header that
    // lies whole within it.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        // SAFETY: as above, and the payload starts right after the header.
        let (header, data) = unsafe { (*cmsg, libc::CMSG_DATA(cmsg)) };
        // `size_t` on glibc, `socklen_t` on musl.
        #[allow(clippy::unnecessary_cast)]
        let len = header.cmsg_len as usize;
        let offset = |pointer: *const u8| pointer as usize - control.as_ptr() as usize;
        let Some(payload) = offset(cmsg.cast())
            .checked_add(len)
            .and_then(|end| control.get(offset(data)..end))
        else {
            malformed.get_or_insert("invalid SCM_RIGHTS control length");
            break;
        };
        if (header.cmsg_level, header.cmsg_type) == (libc::SOL_SOCKET, libc::SCM_RIGHTS) {
            let fd_size = std::mem::size_of::<RawFd>();
            if !payload.len().is_multiple_of(fd_size) {
                malformed.get_or_insert("SCM_RIGHTS payload is not fd-aligned");
            }
            for fd in payload.chunks_exact(fd_size) {
                match RawFd::from_ne_bytes(fd.try_into().expect("fd-sized")) {
                    // The kernel never reports one; owning it would be unsound.
                    fd if fd < 0 => negative = true,
                    // SAFETY: the caller passes this descriptor's ownership.
                    fd => fds.push(unsafe { OwnedFd::from_raw_fd(fd) }),
                }
            }
        }
        // SAFETY: as for CMSG_FIRSTHDR; the message ends within `control`,
        // so stepping past it cannot overflow.
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }
    let error = malformed
        .or(truncated.then_some("truncated SCM_RIGHTS ancillary data"))
        .or(negative.then_some("negative SCM_RIGHTS descriptor"));
    match error {
        Some(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        None => Ok(fds),
    }
}

/// Receive into `data` and `control`, returning the bytes received and
/// the descriptors that came with them.
fn recv_unix_message(
    fd: RawFd,
    data: &mut [u8],
    control: &mut ControlBuffer,
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let control = control.bytes_mut();
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    // SAFETY: an all-zero `msghdr` is valid: null pointers, zero lengths.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    #[cfg(target_env = "musl")]
    {
        msg.msg_controllen = control.len().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "SCM_RIGHTS control buffer is too large for the platform",
            )
        })?;
    }
    #[cfg(not(target_env = "musl"))]
    {
        msg.msg_controllen = control.len();
    }

    // SAFETY: `msg` describes `iov` and `control`, which outlive the call.
    // MSG_CMSG_CLOEXEC installs the descriptors close-on-exec, as VirtWL
    // installs its own, so no program the process starts inherits them.
    let bytes = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    // recvmsg sets `msg_controllen` to the length of the data it wrote.
    #[allow(clippy::unnecessary_cast)] // `socklen_t` on musl
    let written = msg.msg_controllen as usize;
    let control = &control[..written.min(control.len())];
    let truncated = msg.msg_flags & libc::MSG_CTRUNC != 0;
    // SAFETY: `control` is aligned, and recvmsg installed the descriptors
    // of its SCM_RIGHTS messages for this receive alone.
    let fds = unsafe { decode_unix_control_fds(control, truncated) }?;
    Ok((bytes as usize, fds))
}

impl WaylandConnection {
    /// Wrap a Unix socket, which the connection then owns.
    ///
    /// Fails when the descriptor cannot be made non-blocking or registered
    /// with the event loop; the caller drops only this connection.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        // AsyncFd requires a non-blocking descriptor. F_SETFL replaces the
        // status flags, so keep the existing ones.
        let flags = OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL)?);
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(Self {
            transport: ConnectionTransport::Unix {
                socket: AsyncFd::new(fd)?,
                control: ControlBuffer::new(UNIX_CONTROL_LEN),
            },
            read_buf: Vec::new(),
            read_fds: VecDeque::new(),
            ambiguous_untracked_fd: false,
            max_pending_fds: MAX_PENDING_FDS,
        })
    }

    pub fn new_virtwayland(channel: Arc<VirtWaylandChannel>) -> Self {
        Self {
            transport: ConnectionTransport::VirtWayland {
                channel,
                transaction: Transaction::default(),
            },
            read_buf: Vec::new(),
            read_fds: VecDeque::new(),
            ambiguous_untracked_fd: false,
            max_pending_fds: MAX_PENDING_FDS,
        }
    }

    /// Append one receive to the ordered byte and descriptor buffers.
    ///
    /// Fails once more than [`MAX_PENDING_FDS`] descriptors are pending; the
    /// caller then closes the connection, which closes them.
    fn append_received(&mut self, data: &[u8], fds: Vec<OwnedFd>) -> io::Result<()> {
        Self::append(
            &mut self.read_buf,
            &mut self.read_fds,
            self.max_pending_fds,
            data,
            fds,
        )
    }

    /// [`Self::append_received`] for a receive whose `data` borrows the
    /// transport's buffer.
    fn append(
        read_buf: &mut Vec<u8>,
        read_fds: &mut VecDeque<OwnedFd>,
        max_pending_fds: usize,
        data: &[u8],
        fds: Vec<OwnedFd>,
    ) -> io::Result<()> {
        read_buf.extend_from_slice(data);
        read_fds.extend(fds);
        if read_fds.len() > max_pending_fds {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "peer has {} unconsumed file descriptors; the limit is {}",
                    read_fds.len(),
                    max_pending_fds
                ),
            ));
        }
        Ok(())
    }

    /// Return whether the buffered byte stream contains a complete Wayland
    /// message.
    ///
    /// The proxy may deliberately stop dispatching after a bounded batch so a
    /// busy renderer cannot monopolize the current-thread executor.  The
    /// next loop iteration must be able to distinguish a partial header/body
    /// (which still needs another read) from a complete message already in
    /// `read_buf` (which must be dispatched without waiting for new socket
    /// activity).
    pub(crate) fn has_complete_message(&self) -> bool {
        // An invalid length is "ready" too: dispatching it lets the normal
        // protocol validation report the error instead of spinning on a
        // permanently buffered malformed header. A valid message is ready
        // only once all of its bytes are buffered.
        !matches!(MessageHeader::frame(&self.read_buf), Framing::Partial)
    }

    /// Send one transport submission. Private: everything outgoing goes
    /// through [`Self::send_messages`], which keeps each submission within
    /// [`MAX_FDS_PER_SEND`].
    async fn send(&mut self, data: &[u8], fds: &[OwnedFd]) -> io::Result<()> {
        if data.is_empty() && fds.is_empty() {
            return Ok(());
        }

        match &mut self.transport {
            ConnectionTransport::Unix { socket, .. } => {
                let mut raw_fds = [-1; MAX_FDS_PER_SEND];
                let Some(raw_fds) = raw_fds.get_mut(..fds.len()) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "too many descriptors for one submission",
                    ));
                };
                for (raw, fd) in raw_fds.iter_mut().zip(fds) {
                    *raw = fd.as_raw_fd();
                }
                let mut data_offset = 0;
                let mut fds_to_send: &[RawFd] = raw_fds;

                while data_offset < data.len() || !fds_to_send.is_empty() {
                    let mut guard = socket.writable().await?;

                    let remaining_data = &data[data_offset..];
                    let iov = [IoSlice::new(remaining_data)];

                    let rights = [ControlMessage::ScmRights(fds_to_send)];
                    let cmsgs: &[ControlMessage] =
                        if fds_to_send.is_empty() { &[] } else { &rights };

                    let result = guard.try_io(|inner| {
                        sendmsg::<()>(
                            inner.get_ref().as_raw_fd(),
                            &iov,
                            cmsgs,
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
            ConnectionTransport::VirtWayland {
                channel,
                transaction,
            } => channel.send(data, fds, transaction).await,
        }
    }

    /// Send `messages` in order, in as few submissions as
    /// [`MAX_FDS_PER_SEND`] and [`MAX_BYTES_PER_SEND`] allow, and empty it.
    ///
    /// A submission ends only between messages, so every message's
    /// descriptors travel with its bytes: a receiver must never meet a
    /// message whose descriptors have not arrived yet. Nor does the VirtWL
    /// channel cut a message by bytes, unless it is longer than
    /// [`MAX_BYTES_PER_SEND`] on its own; such a message goes alone.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a message with more descriptors than one
    /// submission carries, which is not sent; the messages before it are.
    /// No Wayland message has that many descriptor arguments. A failed
    /// send drops every message, sent or not.
    pub(crate) async fn send_messages<I>(
        &mut self,
        messages: &mut OutgoingMessages<I>,
    ) -> io::Result<()> {
        self.send_submissions(messages, LastSubmission::Send).await
    }

    /// Like [`Self::send_messages`], but, for [`LastSubmission::Keep`],
    /// leave the last submission in `messages`, for the messages pushed
    /// later to fill.
    pub(crate) async fn send_submissions<I>(
        &mut self,
        messages: &mut OutgoingMessages<I>,
        last: LastSubmission,
    ) -> io::Result<()> {
        let mut sent = (0, 0);
        let mut ends = messages.submission_ends().peekable();
        let result = loop {
            let end = match ends.next() {
                None => break Ok(()),
                Some(Err(error)) => break Err(error),
                Some(Ok(end)) => end,
            };
            if last == LastSubmission::Keep && ends.peek().is_none() {
                break Ok(());
            }
            if let Err(error) = self
                .send(&messages.bytes[sent.0..end.0], &messages.fds[sent.1..end.1])
                .await
            {
                break Err(error);
            }
            sent = end;
        };
        drop(ends);
        match result {
            Ok(()) => messages.forget_sent(sent),
            Err(_) => messages.clear(),
        }
        result
    }

    pub async fn recv(&mut self) -> io::Result<usize> {
        match &mut self.transport {
            ConnectionTransport::Unix { socket, control } => {
                let mut buf = [0u8; 4096];

                loop {
                    let mut guard = socket.readable().await?;

                    let result = guard.try_io(|inner| {
                        recv_unix_message(inner.get_ref().as_raw_fd(), &mut buf, control)
                    });

                    match result {
                        Ok(Ok((bytes, fds))) => {
                            self.append_received(&buf[..bytes], fds)?;
                            return Ok(bytes);
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(_would_block) => continue,
                    }
                }
            }
            ConnectionTransport::VirtWayland {
                channel,
                transaction,
            } => {
                let (data, fds) = channel.recv(transaction).await?;
                Self::append(
                    &mut self.read_buf,
                    &mut self.read_fds,
                    self.max_pending_fds,
                    data,
                    fds,
                )?;
                Ok(data.len())
            }
        }
    }

    /// Attempt one receive without waiting for transport readiness.
    ///
    /// `Ok(None)` means the non-blocking transport has no data. Any received
    /// bytes and descriptors are appended to the same ordered buffers used by
    /// [`Self::recv`].
    pub(crate) fn try_recv(&mut self) -> io::Result<Option<usize>> {
        match &mut self.transport {
            ConnectionTransport::Unix { socket, control } => {
                let mut buf = [0u8; 4096];
                match recv_unix_message(socket.as_raw_fd(), &mut buf, control) {
                    Ok((bytes, fds)) => {
                        self.append_received(&buf[..bytes], fds)?;
                        Ok(Some(bytes))
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
                    Err(error) => Err(error),
                }
            }
            ConnectionTransport::VirtWayland {
                channel,
                transaction,
            } => match channel.try_recv(transaction)? {
                Some((data, fds)) => {
                    Self::append(
                        &mut self.read_buf,
                        &mut self.read_fds,
                        self.max_pending_fds,
                        data,
                        fds,
                    )?;
                    Ok(Some(data.len()))
                }
                None => Ok(None),
            },
        }
    }

    /// The socket of a Unix connection; a VirtWL connection has none.
    #[cfg(test)]
    pub(crate) fn unix_socket_for_test(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        match &self.transport {
            ConnectionTransport::Unix { socket, .. } => {
                Some(std::os::fd::AsFd::as_fd(socket.get_ref()))
            }
            ConnectionTransport::VirtWayland { .. } => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::{
        decode_unix_control_fds, recv_unix_message, ControlBuffer, LastSubmission,
        OutgoingMessages, WaylandConnection, MAX_BYTES_PER_SEND, MAX_FDS_PER_SEND,
        UNIX_MAX_SCM_RIGHTS_FDS,
    };
    use crate::test_support::{any_fd_has_identity, fd_identity, fds_with_identity, shm_pool_file};
    use crate::wire::{MessageBuilder, Outgoing};
    use nix::fcntl::{fcntl, FcntlArg, FdFlag};
    use nix::sys::memfd::{memfd_create, MFdFlags};
    use nix::sys::socket::{sendmsg, setsockopt, sockopt, ControlMessage, MsgFlags};
    use std::ffi::CString;
    use std::fs;
    use std::io::IoSlice;
    use std::io::Write;
    use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;

    /// A send to a peer that has gone returns `BrokenPipe` and raises no
    /// `SIGPIPE`, even where `SIGPIPE` would end the process. The Rust
    /// runtime ignores it, so the test restores its default action, which
    /// is the process's, in a child process of its own.
    #[test]
    fn unix_send_suppresses_sigpipe() {
        crate::test_support::in_own_process(
            "connection::tests::unix_send_suppresses_sigpipe",
            || {
                assert_ne!(
                    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) },
                    libc::SIG_ERR
                );
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build a runtime");
                let error = runtime.block_on(async {
                    let (peer, socket) = UnixStream::pair().expect("unix socket pair");
                    drop(peer);
                    let mut connection =
                        WaylandConnection::new(socket.into()).expect("register test socket");
                    let mut messages = OutgoingMessages::default();
                    messages.push(Outgoing::from_parts(vec![0; 8], Vec::new()));
                    connection
                        .send_messages(&mut messages)
                        .await
                        .expect_err("the peer has gone")
                });
                assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            },
        );
    }

    /// Once a connection's buffers have grown, receiving a message
    /// allocates nothing, as the control buffer is the connection's, and
    /// sending one with a descriptor allocates only the control message
    /// nix's `sendmsg` builds itself: the descriptor array is on the stack.
    #[tokio::test]
    async fn warm_receives_and_sends_allocate_no_buffers_of_their_own() {
        use crate::test_support::counting::Counts;
        use std::io::Write;

        let (mut peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let name = CString::new("sommelier-warm-send").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");
        let message = MessageBuilder::new().build_message(1, 0);
        let mut costs = Vec::new();
        for _ in 0..2 {
            peer.write_all(&message).expect("send to the proxy");
            let before = Counts::now();
            assert_eq!(connection.try_recv().expect("receive"), Some(message.len()));
            let received = Counts::now() - before;
            connection.read_buf.clear();

            let mut messages = OutgoingMessages::default();
            messages.push(Outgoing::from_parts(
                message.clone(),
                vec![memfd.try_clone().expect("dup")],
            ));
            let before = Counts::now();
            connection
                .send_messages(&mut messages)
                .await
                .expect("send to the peer");
            costs.push((received.allocations, (Counts::now() - before).allocations));

            let mut data = [0u8; 64];
            let mut control = ControlBuffer::new(4096);
            let (bytes, fds) =
                recv_unix_message(peer.as_raw_fd(), &mut data, &mut control).expect("receive");
            assert_eq!((bytes, fds.len()), (message.len(), 1));
        }
        assert_eq!(
            costs[1],
            (0, 1),
            "allocations to receive, then to send: {costs:?}"
        );
    }

    #[tokio::test]
    async fn try_recv_is_nonblocking_and_preserves_the_ordered_stream() {
        let (mut peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");

        assert_eq!(
            connection.try_recv().expect("empty non-blocking receive"),
            None
        );

        let message = [1u8, 2, 3, 4, 5, 6, 7, 8];
        peer.write_all(&message).expect("write peer message");
        assert_eq!(
            connection.try_recv().expect("ready non-blocking receive"),
            Some(message.len())
        );
        assert_eq!(connection.read_buf, message);
        assert!(connection.read_fds.is_empty());
    }

    #[tokio::test]
    async fn unregistrable_descriptor_is_an_error_not_a_panic() {
        // epoll refuses regular files, so the event loop cannot watch one.
        let name = CString::new("sommelier-unpollable").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");
        assert!(WaylandConnection::new(memfd).is_err());
    }

    /// Send one byte with `fds` attached, as a message that takes no
    /// descriptor would carry them.
    fn send_with_fds(peer: &UnixStream, fds: &[RawFd]) {
        let sent = sendmsg::<()>(
            peer.as_raw_fd(),
            &[IoSlice::new(b"x")],
            &[ControlMessage::ScmRights(fds)],
            MsgFlags::empty(),
            None,
        )
        .expect("send descriptors");
        assert_eq!(sent, 1);
    }

    #[tokio::test]
    async fn send_messages_splits_only_between_messages() {
        let (peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let name = CString::new("sommelier-split-batch").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");

        // (bytes, descriptors) per message: the second message would take
        // the first submission past the limit, so it starts the next one.
        let mut messages = OutgoingMessages::default();
        for (len, fds) in [(8, 20), (12, 10), (16, 0), (20, 5)] {
            let fds = (0..fds)
                .map(|_| memfd.try_clone().expect("dup the memfd"))
                .collect();
            messages.push(Outgoing::from_parts(vec![0; len], fds));
        }
        connection
            .send_messages(&mut messages)
            .await
            .expect("send the batch");
        drop(connection);

        let mut submissions = Vec::new();
        loop {
            let mut data = [0u8; 4096];
            let mut control = ControlBuffer::new(4096);
            let (bytes, fds) =
                recv_unix_message(peer.as_raw_fd(), &mut data, &mut control).expect("receive");
            if bytes == 0 {
                break;
            }
            submissions.push((bytes, fds.len()));
        }
        assert_eq!(submissions, vec![(8, 20), (48, 15)]);
    }

    /// While more messages may follow, only the full submissions go out:
    /// the last one waits for the messages that could still fill it, and
    /// goes out with them.
    #[tokio::test]
    async fn kept_submissions_wait_for_the_messages_that_fill_them() {
        let (peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let name = CString::new("sommelier-full-submissions").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");
        let message = |len: usize, fds: usize| {
            let fds = (0..fds)
                .map(|_| memfd.try_clone().expect("dup the memfd"))
                .collect();
            Outgoing::from_parts(vec![0; len], fds)
        };

        // (bytes, descriptors) per message. Each submission starts with a
        // descriptor, so the receiver reads each one on its own.
        let limit = MAX_BYTES_PER_SEND;
        let mut messages = OutgoingMessages::default();
        messages.push(message(limit - 8, 1));
        messages.push(message(12, 1));
        for _ in 0..2 {
            connection
                .send_submissions(&mut messages, LastSubmission::Keep)
                .await
                .expect("send the full submission");
        }
        messages.push(message(8, 0));
        connection
            .send_messages(&mut messages)
            .await
            .expect("send the rest");
        assert!(messages.is_empty());
        drop(connection);

        let mut submissions = Vec::new();
        loop {
            let mut data = [0u8; 4096];
            let mut control = ControlBuffer::new(4096);
            let (bytes, fds) =
                recv_unix_message(peer.as_raw_fd(), &mut data, &mut control).expect("receive");
            if bytes == 0 {
                break;
            }
            submissions.push((bytes, fds.len()));
        }
        assert_eq!(submissions, vec![(limit - 8, 1), (20, 1)]);
    }

    /// A VirtWL transaction holds at most `MAX_BYTES_PER_SEND` bytes, and
    /// the channel cuts a longer submission by bytes. A submission must
    /// therefore end at a message boundary within that bound, so no cut
    /// falls inside a message: Exo could dispatch its two parts in
    /// different reads. A message longer than the bound goes alone.
    #[tokio::test]
    async fn send_messages_ends_submissions_at_message_boundaries_within_the_byte_limit() {
        let (peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let name = CString::new("sommelier-split-bytes").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");

        // (bytes, descriptors) per message. A cut every `limit` bytes would
        // fall 8 bytes into the second message, sync-sized. Each submission
        // starts with a descriptor, so the receiver reads each one on its
        // own.
        let limit = MAX_BYTES_PER_SEND;
        let longest = crate::wire::MessageHeader::MAX_LEN;
        let mut messages = OutgoingMessages::default();
        for (len, fds) in [(limit - 8, 1), (12, 0), (12, 1), (longest, 1), (8, 1)] {
            let fds = (0..fds)
                .map(|_| memfd.try_clone().expect("dup the memfd"))
                .collect();
            messages.push(Outgoing::from_parts(vec![0; len], fds));
        }
        connection
            .send_messages(&mut messages)
            .await
            .expect("send the batch");
        drop(connection);

        let mut submissions = Vec::new();
        loop {
            let mut data = [0u8; 4 * 4096];
            let mut control = ControlBuffer::new(4096);
            let (bytes, fds) =
                recv_unix_message(peer.as_raw_fd(), &mut data, &mut control).expect("receive");
            if bytes == 0 {
                break;
            }
            submissions.push((bytes, fds.len()));
        }
        assert_eq!(
            submissions,
            vec![(limit - 8, 1), (24, 1), (longest, 1), (8, 1)]
        );
    }

    #[tokio::test]
    async fn a_message_with_more_descriptors_than_a_submission_is_not_sent() {
        let (peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let name = CString::new("sommelier-oversized-message").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");
        let identity = fd_identity(memfd.as_raw_fd());

        // A valid message, then one whose descriptors cannot travel with it.
        let mut messages = OutgoingMessages::default();
        for (len, fds) in [(8, 1), (12, MAX_FDS_PER_SEND + 1)] {
            let fds = (0..fds)
                .map(|_| memfd.try_clone().expect("dup the memfd"))
                .collect();
            messages.push(Outgoing::from_parts(vec![0; len], fds));
        }
        drop(memfd);
        let error = connection
            .send_messages(&mut messages)
            .await
            .expect_err("the second message cannot be sent");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        drop(messages);
        drop(connection);

        let mut submissions = Vec::new();
        loop {
            let mut data = [0u8; 64];
            let mut control = ControlBuffer::new(4096);
            let (bytes, fds) =
                recv_unix_message(peer.as_raw_fd(), &mut data, &mut control).expect("receive");
            if bytes == 0 {
                break;
            }
            submissions.push((bytes, fds.len()));
        }
        assert_eq!(submissions, [(8, 1)], "only the message before it is sent");
        assert!(!any_fd_has_identity(identity));
    }

    #[tokio::test]
    async fn too_many_unconsumed_fds_end_the_connection() {
        let (peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        // A small limit stands in for MAX_PENDING_FDS, and one descriptor
        // already pending brings the queue to one below it.
        connection.max_pending_fds = 2;
        connection.read_fds.push_back(shm_pool_file(&[]).into());

        let name = CString::new("sommelier-pending-fd-limit").unwrap();
        let memfd = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");
        let identity = fd_identity(memfd.as_raw_fd());
        send_with_fds(&peer, &[memfd.as_raw_fd()]);
        send_with_fds(&peer, &[memfd.as_raw_fd()]);
        drop(memfd);

        assert_eq!(
            connection
                .recv()
                .await
                .expect("the limit itself is allowed"),
            1
        );
        assert!(
            connection.recv().await.is_err(),
            "one descriptor past the limit must end the connection"
        );
        drop(connection);
        assert!(
            !any_fd_has_identity(identity),
            "closing the connection must close every pending descriptor"
        );
    }

    #[tokio::test]
    async fn drops_unconsumed_received_fds() {
        let (read, _write) = crate::test_support::pipe();
        // Use a memfd for the received descriptor so its `(device, inode)`
        // identity is not shared with the connection's pipe peer or another
        // anonymous-inode object. Eventfds all report one inode on Linux.
        let name = CString::new("sommelier-connection-fd-test").unwrap();
        let pending_fd =
            memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create");
        let pending_identity = fd_identity(pending_fd.as_raw_fd());
        let mut connection = WaylandConnection::new(read).expect("register test pipe");
        connection.read_fds.push_back(pending_fd);
        drop(connection);

        // The numeric descriptor can be reused immediately by another test,
        // so check the memfd's identity rather than the integer.
        assert!(
            !any_fd_has_identity(pending_identity),
            "dropping the connection must close the received descriptor"
        );
    }

    fn memfd(name: &str) -> OwnedFd {
        let name = CString::new(name).unwrap();
        memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC).expect("memfd_create")
    }

    /// The payload of an `SCM_RIGHTS` message that passes `fds`.
    fn fd_payload<const N: usize>(fds: [OwnedFd; N]) -> Vec<u8> {
        fds.into_iter()
            .flat_map(|fd| fd.into_raw_fd().to_ne_bytes())
            .collect()
    }

    /// Control data holding `messages`, each a level, a type and a payload,
    /// laid out as `recvmsg` writes it.
    fn control_data(messages: &[(libc::c_int, libc::c_int, &[u8])]) -> Vec<u8> {
        let mut control = Vec::new();
        for &(level, kind, payload) in messages {
            let start = control.len();
            let n = payload.len() as u32;
            // SAFETY: CMSG_SPACE and CMSG_LEN only compute lengths, and an
            // all-zero `cmsghdr` is valid.
            let (space, len, mut header) = unsafe {
                let header: libc::cmsghdr = std::mem::zeroed();
                (libc::CMSG_SPACE(n) as usize, libc::CMSG_LEN(n), header)
            };
            header.cmsg_len = len as _;
            header.cmsg_level = level;
            header.cmsg_type = kind;
            control.resize(start + space, 0);
            // SAFETY: the header fits in the space just added.
            unsafe {
                control[start..]
                    .as_mut_ptr()
                    .cast::<libc::cmsghdr>()
                    .write_unaligned(header);
            }
            let data = start + std::mem::size_of::<libc::cmsghdr>();
            control[data..data + payload.len()].copy_from_slice(payload);
        }
        control
    }

    /// Take over the descriptors in `control`, which this test passes.
    fn decode(control: &[u8], truncated: bool) -> std::io::Result<Vec<OwnedFd>> {
        let mut aligned = ControlBuffer::new(control.len());
        aligned.bytes_mut().copy_from_slice(control);
        // SAFETY: a `ControlBuffer` is aligned for `cmsghdr`, and the tests
        // build control data from descriptors they pass on.
        unsafe { decode_unix_control_fds(aligned.bytes_mut(), truncated) }
    }

    #[test]
    fn truncated_scm_rights_closes_descriptors_recovered_from_control_buffer() {
        let received = memfd("sommelier-truncated-control");
        let identity = fd_identity(received.as_raw_fd());
        let payload = fd_payload([received]);
        let control = control_data(&[(libc::SOL_SOCKET, libc::SCM_RIGHTS, &payload)]);

        let error = decode(&control, true).expect_err("truncated control data");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            !any_fd_has_identity(identity),
            "truncated control cleanup must release the received fd"
        );
    }

    #[test]
    fn negative_scm_rights_descriptor_is_invalid_data() {
        // The kernel never reports one; owning it would be unsound. The
        // descriptor after it must still be owned, and closed.
        let received = memfd("sommelier-after-negative-fd");
        let identity = fd_identity(received.as_raw_fd());
        let mut payload = (-1 as RawFd).to_ne_bytes().to_vec();
        payload.extend(fd_payload([received]));
        let control = control_data(&[(libc::SOL_SOCKET, libc::SCM_RIGHTS, &payload)]);

        let error = decode(&control, false).expect_err("a negative descriptor");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            !any_fd_has_identity(identity),
            "the descriptors after a negative one must be closed"
        );
    }

    /// Every `SCM_RIGHTS` message yields its descriptors, in order, past an
    /// empty one and past messages of another type or level, whose
    /// payloads, read as descriptors, would be negative.
    #[test]
    fn every_scm_rights_message_yields_its_descriptors_in_order() {
        let files = ["a", "b", "c"].map(|name| memfd(&format!("sommelier-cmsg-{name}")));
        let identities = files.each_ref().map(|fd| fd_identity(fd.as_raw_fd()));
        let [a, b, c] = files;
        let credentials = [0xff; std::mem::size_of::<libc::ucred>()];
        let control = control_data(&[
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &credentials),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, &[]),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, &fd_payload([a])),
            (libc::SOL_IP, libc::SCM_RIGHTS, &[0xff; 4]),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, &fd_payload([b, c])),
        ]);

        let fds = decode(&control, false).expect("valid control data");
        let received: Vec<_> = fds.iter().map(|fd| fd_identity(fd.as_raw_fd())).collect();
        assert_eq!(received, identities);
        drop(fds);
        assert!(identities.into_iter().all(|id| !any_fd_has_identity(id)));
    }

    /// A message whose length runs past the control data ends the receive,
    /// and the descriptors of the messages before it close.
    #[test]
    fn control_length_past_the_buffer_is_invalid_data() {
        let received = memfd("sommelier-before-bad-length");
        let identity = fd_identity(received.as_raw_fd());
        let mut control = control_data(&[
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, &fd_payload([received])),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, &[0xff; 8]),
        ]);
        control.truncate(control.len() - 4);

        let error = decode(&control, false).expect_err("a length past the buffer");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!any_fd_has_identity(identity));
    }

    /// A socket pair whose second end receives the sender's credentials,
    /// which the kernel writes before the descriptors, in a message of
    /// their own.
    fn credentials_pair() -> (UnixStream, UnixStream) {
        let (sender, receiver) = UnixStream::pair().expect("unix socket pair");
        setsockopt(&receiver, sockopt::PassCred, &true).expect("SO_PASSCRED");
        (sender, receiver)
    }

    #[test]
    fn recvmsg_takes_the_descriptors_after_a_credentials_message() {
        let (sender, receiver) = credentials_pair();
        let file = memfd("sommelier-after-credentials");
        let identity = fd_identity(file.as_raw_fd());
        send_with_fds(&sender, &[file.as_raw_fd(); 3]);

        let (mut data, mut control) = ([0u8; 1], ControlBuffer::new(4096));
        let (bytes, fds) =
            recv_unix_message(receiver.as_raw_fd(), &mut data, &mut control).expect("receive");
        assert_eq!((bytes, fds.len()), (1, 3));
        drop((file, fds));
        assert!(!any_fd_has_identity(identity));
    }

    /// The largest batch Linux sends, `SCM_MAX_FD` descriptors, fits in a
    /// connection's control buffer, so a valid peer never causes truncation.
    #[tokio::test]
    async fn a_connection_receives_scm_max_fd_descriptors_whole() {
        let (peer, socket) = UnixStream::pair().expect("unix socket pair");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let file = memfd("sommelier-scm-max-fd");
        send_with_fds(&peer, &[file.as_raw_fd(); UNIX_MAX_SCM_RIGHTS_FDS]);

        assert_eq!(connection.try_recv().expect("receive"), Some(1));
        assert_eq!(connection.read_fds.len(), UNIX_MAX_SCM_RIGHTS_FDS);
    }

    /// A received descriptor is close-on-exec from the moment the kernel
    /// installs it, so no program the process starts inherits a peer's
    /// file.
    #[test]
    fn received_descriptors_close_on_exec() {
        let (sender, receiver) = UnixStream::pair().expect("unix socket pair");
        let file = memfd("sommelier-received-cloexec");
        send_with_fds(&sender, &[file.as_raw_fd()]);

        let (mut data, mut control) = ([0u8; 1], ControlBuffer::new(4096));
        let (_, fds) =
            recv_unix_message(receiver.as_raw_fd(), &mut data, &mut control).expect("receive");
        let [fd] = fds.try_into().expect("one descriptor");
        let flags = FdFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFD).expect("F_GETFD"));
        assert!(flags.contains(FdFlag::FD_CLOEXEC));
    }

    /// A control buffer with room for the credentials and two of three
    /// descriptors: the kernel installs the two, and both close before the
    /// error returns.
    #[test]
    fn recvmsg_truncation_after_a_credentials_message_closes_every_descriptor() {
        let (sender, receiver) = credentials_pair();
        let file = memfd("sommelier-truncated-after-credentials");
        let identity = fd_identity(file.as_raw_fd());
        send_with_fds(&sender, &[file.as_raw_fd(); 3]);

        // SAFETY: CMSG_SPACE only computes a length.
        let space = |len: usize| unsafe { libc::CMSG_SPACE(len as u32) } as usize;
        let room =
            space(std::mem::size_of::<libc::ucred>()) + space(2 * std::mem::size_of::<RawFd>());
        let (mut data, mut control) = ([0u8; 1], ControlBuffer::new(room));
        let error = recv_unix_message(receiver.as_raw_fd(), &mut data, &mut control)
            .expect_err("truncated control data");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            fds_with_identity(identity),
            1,
            "only the sender's descriptor may refer to the file"
        );
    }

    #[test]
    fn recvmsg_scm_rights_truncation_closes_kernel_installed_descriptors() {
        let (sender, receiver) = UnixStream::pair().expect("Unix socket pair should be created");
        let sender_fd = sender.into_raw_fd();
        let receiver_fd = receiver.into_raw_fd();

        let (pipe_read, pipe_write) = crate::test_support::pipe();
        let pipe_target = fs::read_link(format!("/proc/self/fd/{}", pipe_read.as_raw_fd()))
            .expect("pipe descriptor should be visible through procfs");
        let sent_fds = [pipe_read.as_raw_fd(); 4];

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
        let mut control = ControlBuffer::new(control_len);
        let result = recv_unix_message(receiver_fd, &mut data, &mut control);
        assert!(
            result.is_err(),
            "a control buffer smaller than the SCM_RIGHTS batch must report truncation"
        );

        // Close the sender's originals. Any descriptor still referring to the
        // pipe after this point was installed by recvmsg and leaked past the
        // truncation cleanup path.
        drop((pipe_read, pipe_write));
        unsafe {
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

    #[tokio::test]
    async fn buffered_message_readiness_distinguishes_partial_and_complete_frames() {
        let (socket, _peer) = UnixStream::pair().expect("Unix socket pair should be created");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");

        // Opcode 0 keeps the bytes that hold the opcode zero, so a decoder
        // that reads the length from the opcode half sees an invalid length
        // and reports a partial frame as ready.
        let mut builder = MessageBuilder::new();
        builder.write_u32(2);
        let message = builder.build_message(1, 0);
        for partial in 0..message.len() {
            connection.read_buf.clear();
            connection.read_buf.extend_from_slice(&message[..partial]);
            assert!(
                !connection.has_complete_message(),
                "{partial} of {} bytes must wait for the rest",
                message.len()
            );
        }
        connection.read_buf.clear();
        connection.read_buf.extend_from_slice(&message);
        assert!(connection.has_complete_message());
    }

    #[tokio::test]
    async fn malformed_buffered_length_is_ready_for_protocol_validation() {
        let (socket, _peer) = UnixStream::pair().expect("Unix socket pair should be created");
        let mut connection = WaylandConnection::new(socket.into()).expect("register test socket");
        let mut message = MessageBuilder::new().build_message(1, 0);
        // Rewrite the length to 6, below the header size.
        let invalid_word = (6_u32 << 16).to_ne_bytes();
        message[4..8].copy_from_slice(&invalid_word);
        connection.read_buf.extend_from_slice(&message);
        assert!(
            connection.has_complete_message(),
            "invalid buffered headers must not make the event loop wait forever"
        );
    }
}
