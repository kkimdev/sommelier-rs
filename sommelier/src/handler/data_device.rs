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

use crate::protocols::wayland::wl_data_device;
use crate::protocols::wayland::wl_data_device_manager;
use crate::protocols::wayland::wl_data_offer;
use crate::protocols::wayland::wl_data_source;
use crate::state::Context;
use crate::wire::{queue_or_drop, Action};
use log::{debug, error};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::socket::{send, MsgFlags};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

pub struct DataDeviceHandler;

impl wl_data_device_manager::WlDataDeviceManagerHandler for DataDeviceHandler {}

impl wl_data_device::WlDataDeviceHandler for DataDeviceHandler {}

impl wl_data_offer::WlDataOfferHandler for DataDeviceHandler {
    fn on_receive(&mut self, ctx: &mut Context, mime_type: &String, fd: BorrowedFd<'_>) -> Action {
        debug!(
            "wl_data_offer.receive: mime_type={}, fd={}",
            mime_type,
            fd.as_raw_fd()
        );

        if let Some(virtwl) = &ctx.virtwayland_channel {
            debug!("Using virtwl for clipboard receive");

            // Beyond the cap the request is dropped, which closes the
            // guest's descriptor, so the guest reads EOF.
            let Some(pump_slot) = ctx.clipboard_pumps.slot() else {
                return Action::Drop;
            };

            // The request closes the guest's descriptor once handled, so the
            // pump writes into a duplicate. Duplicate it before queueing
            // anything so a dup failure cannot leave a host request queued
            // while the original request is also forwarded.
            let client_fd = match fd.try_clone_to_owned() {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to dup client fd: {}", e);
                    return Action::Drop;
                }
            };

            // Create a pipe where we read (so host must write to it)
            // VIRTWL_IOCTL_NEW_PIPE_READ creates a pipe that is readable via the returned FD.
            let virtwl_pipe = match virtwl.create_read_pipe() {
                Ok(p) => p,
                Err(e) => {
                    error!("Failed to create virtwl pipe: {}", e);
                    return Action::Drop;
                }
            };

            // We need to send this virtwl FD to the host.
            // The guest client provided 'fd' to write data into.
            // We pump: virtwl_pipe (read) -> fd (write)

            let sender_id = ctx.last_sender_id;
            let Some(host_sender_id) = ctx.shadow_table.get_host_id(sender_id) else {
                error!(
                    "Dropping wl_data_offer.receive from unmapped sender {}",
                    sender_id
                );
                return Action::Drop;
            };
            debug!(
                "on_receive: sender_id={}, host_sender_id={}",
                sender_id, host_sender_id
            );

            // The host writes into a duplicate of the pipe, and the pump
            // below reads from the original.
            let virtwl_fd_dup = match virtwl_pipe.try_clone() {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to dup virtwl fd: {}", e);
                    return Action::Drop;
                }
            };
            if !queue_or_drop(
                &mut ctx.client_to_host_queue,
                wl_data_offer::req::receive(host_sender_id, mime_type, virtwl_fd_dup),
                "an oversized clipboard MIME type",
            ) {
                return Action::Drop;
            }

            pump_slot.start(async move {
                match pump(virtwl_pipe, client_fd).await {
                    Ok(()) => debug!("Clipboard pump finished"),
                    Err(e) => error!("Clipboard pump error: {}", e),
                }
            });

            // Drop original action (we handled it)
            return Action::Drop;
        }

        Action::Forward
    }
}

impl wl_data_source::WlDataSourceHandler for DataDeviceHandler {}

/// How many bytes a clipboard pump reads at a time.
const PUMP_BUFFER_LEN: usize = 64 * 1024;

/// How long a clipboard transfer may go without moving a byte, the first
/// one included, before its pump ends it. A host that never finishes
/// writing, or a guest that never reads, would otherwise keep one of the
/// connection's pump slots, and any host data left unread in VirtWL's
/// shared receive queue, until the connection ends.
///
/// The guest's pipe then closes as it does after a whole transfer: a pipe
/// has no way to report an error, so the guest cannot tell a paste cut off
/// here from a complete one.
const PUMP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Copy `src`, the proxy's VirtWL pipe, into `dst`, the guest's
/// descriptor, until `src` ends or `dst`'s reader goes away.
///
/// Every client shares one thread, so the pump waits on the event loop: a
/// peer that stalls parks only this pump until [`PUMP_IDLE_TIMEOUT`] ends
/// it, and aborting the pump closes both descriptors without waiting on
/// either peer. Tokio's pipe types would take only a FIFO, and a VirtWL
/// pipe is not one, but `virtio_wl.c` implements `poll` for it and returns
/// `EAGAIN` under `O_NONBLOCK`.
pub(crate) async fn pump(src: OwnedFd, dst: OwnedFd) -> io::Result<()> {
    match copy(src, dst).await {
        // The guest closed its end and wants no more. Rust ignores SIGPIPE,
        // so a pipe write fails instead of ending the process.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

async fn copy(src: OwnedFd, dst: OwnedFd) -> io::Result<()> {
    let mut dst = Destination::new(dst)?;
    // The duplicate of the VirtWL pipe sent to the host is closed once
    // sent, so this description is the proxy's alone to make non-blocking.
    let flags = OFlag::from_bits_truncate(fcntl(&src, FcntlArg::F_GETFL)?);
    fcntl(&src, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    // A VirtWL pipe's `poll` also waits on the device's shared send queue,
    // so watching it for writing too would only wake the pump for nothing.
    let src = AsyncFd::with_interest(File::from(src), Interest::READABLE)?;
    let mut buf = vec![0; PUMP_BUFFER_LEN];
    loop {
        let len = wait_io(&src, Interest::READABLE, |mut src| src.read(&mut buf)).await?;
        if len == 0 {
            return Ok(());
        }
        let mut unwritten = &buf[..len];
        while !unwritten.is_empty() {
            match dst.write(unwritten).await? {
                // A write that makes no progress would spin on the loop.
                0 => return Err(io::ErrorKind::WriteZero.into()),
                written => unwritten = &unwritten[written..],
            }
        }
    }
}

/// Where a pump writes.
///
/// The pump never changes the flags of the guest's open file description:
/// it is shared with the client, and with any other pump into the same
/// pipe, so a shared `O_NONBLOCK` could be cleared under a pump and leave
/// its next write blocking the loop.
enum Destination {
    /// A pipe or a named FIFO.
    Pipe(GuestPipe),
    /// A socket, written with `MSG_DONTWAIT`.
    Socket(AsyncFd<File>),
    /// A regular file, such as a memfd, written directly. A local file
    /// never waits on another process, but one on a network or FUSE file
    /// system (virtiofs, 9p, NFS) can, and no flag prevents that. Its
    /// writes are the only ones [`PUMP_IDLE_TIMEOUT`] does not bound: they
    /// never report "would block", so there is no wait to time out.
    File(File),
}

impl Destination {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let file = File::from(fd);
        let kind = file.metadata()?.file_type();
        if kind.is_fifo() {
            Ok(Self::Pipe(GuestPipe {
                fd: AsyncFd::with_interest(file, Interest::WRITABLE)?,
                reopened: false,
            }))
        } else if kind.is_socket() {
            Ok(Self::Socket(AsyncFd::with_interest(
                file,
                Interest::WRITABLE,
            )?))
        } else if kind.is_file() {
            Ok(Self::File(file))
        } else {
            // A device's write may sleep, and epoll cannot say when.
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the guest's descriptor is not a pipe, a socket or a regular file",
            ))
        }
    }

    /// Write some of `data`, once the destination takes it.
    async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Self::Pipe(pipe) => pipe.write(data).await,
            Self::Socket(socket) => {
                let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL;
                let send = |socket: &File| Ok(send(socket.as_raw_fd(), data, flags)?);
                wait_io(socket, Interest::WRITABLE, send).await
            }
            Self::File(file) => retry_interrupted(|| {
                let mut file: &File = file;
                file.write(data)
            }),
        }
    }
}

/// The guest's pipe.
///
/// An anonymous pipe on Linux 6.5 or later is written with `RWF_NOWAIT`,
/// which makes each write non-blocking whatever the flags of the pipe's
/// description. A named FIFO, or any pipe on an older kernel, refuses that
/// flag, so it is reopened instead through `/proc/self/fd` as a private
/// non-blocking description. Opening needs write permission on the pipe,
/// which a client running as another user (started through `sudo -E`, say)
/// may not give the proxy; its transfer is then refused.
struct GuestPipe {
    fd: AsyncFd<File>,
    reopened: bool,
}

impl GuestPipe {
    async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if !self.reopened {
            let write = |pipe: &File| write_nowait(pipe, data);
            match wait_io(&self.fd, Interest::WRITABLE, write).await {
                Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => {
                    let pipe = reopen(self.fd.get_ref())?;
                    self.fd = AsyncFd::with_interest(pipe, Interest::WRITABLE)?;
                    self.reopened = true;
                }
                result => return result,
            }
        }
        wait_io(&self.fd, Interest::WRITABLE, |mut pipe| pipe.write(data)).await
    }
}

/// Write some of `data` into `pipe` without waiting for room: `EAGAIN`
/// when it is full, whatever the flags of its description.
fn write_nowait(pipe: &File, data: &[u8]) -> io::Result<usize> {
    let iov = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: data.len(),
    };
    // SAFETY: `iov` describes `data`, which outlives the call, and the
    // kernel only reads it. Offset -1 writes at the current position, which
    // is all a pipe has.
    let written = unsafe { libc::pwritev2(pipe.as_raw_fd(), &iov, 1, -1, libc::RWF_NOWAIT) };
    usize::try_from(written).map_err(|_| io::Error::last_os_error())
}

/// Open the pipe behind `pipe` again as a private non-blocking description.
fn reopen(pipe: &File) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(format!("/proc/self/fd/{}", pipe.as_raw_fd()))
        .map_err(|e| match e.raw_os_error() {
            // A named FIFO whose reader is gone cannot be opened again. An
            // anonymous pipe can, and its first write then fails with EPIPE.
            Some(libc::ENXIO) => io::ErrorKind::BrokenPipe.into(),
            _ => io::Error::new(e.kind(), format!("cannot reopen the guest's pipe: {e}")),
        })
}

/// Run `op` on `fd` once it is ready for `interest`, and again whenever it
/// would block or a signal interrupts it, for up to [`PUMP_IDLE_TIMEOUT`].
async fn wait_io<R>(
    fd: &AsyncFd<File>,
    interest: Interest,
    mut op: impl FnMut(&File) -> io::Result<R>,
) -> io::Result<R> {
    let ready = fd.async_io(interest, |file| retry_interrupted(|| op(file)));
    tokio::time::timeout(PUMP_IDLE_TIMEOUT, ready)
        .await
        .map_err(|_elapsed| {
            let idle = PUMP_IDLE_TIMEOUT.as_secs();
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no data moved for {idle} s"),
            )
        })?
}

/// Run `op` until a signal does not interrupt it.
fn retry_interrupted<R>(mut op: impl FnMut() -> io::Result<R>) -> io::Result<R> {
    loop {
        match op() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{pump, reopen, write_nowait, DataDeviceHandler, PUMP_IDLE_TIMEOUT};
    use crate::protocols::wayland::wl_data_device;
    use crate::state::Context;
    use crate::test_support::{raw_dispatch, Rng};
    use crate::wire::WireMessage;
    use std::io::{self, Read, Write};
    use std::thread;
    use std::time::Duration;

    /// Write `data` into `writer` on a thread of its own, then close it.
    fn write_on_a_thread(mut writer: io::PipeWriter, data: &[u8]) -> thread::JoinHandle<()> {
        let data = data.to_vec();
        thread::spawn(move || writer.write_all(&data).expect("write the transfer"))
    }

    /// Read `reader` to its end on a thread of its own, `chunk` bytes at a
    /// time.
    fn read_on_a_thread(
        mut reader: impl Read + Send + 'static,
        chunk: usize,
    ) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let mut received = Vec::new();
            let mut buf = vec![0; chunk];
            loop {
                match reader.read(&mut buf).expect("read the transfer") {
                    0 => return received,
                    len => received.extend_from_slice(&buf[..len]),
                }
            }
        })
    }

    /// Wait for a peer thread, once the pump it feeds or drains has ended.
    // The test's own thread waits here, not the event loop.
    #[allow(clippy::disallowed_methods)]
    fn join<T>(thread: thread::JoinHandle<T>) -> T {
        thread.join().expect("the peer thread does not panic")
    }

    /// A transfer many times the size of a pipe and of the pump's buffer
    /// arrives whole and in order, and the pump ends at the source's EOF.
    #[tokio::test]
    async fn pump_copies_a_large_transfer_whole() {
        let data = Rng::new(1).bytes(8 << 20);
        let (src, src_writer) = io::pipe().unwrap();
        let (dst_reader, dst) = io::pipe().unwrap();
        let writer = write_on_a_thread(src_writer, &data);
        let reader = read_on_a_thread(dst_reader, 1 << 20);

        pump(src.into(), dst.into()).await.unwrap();

        join(writer);
        assert!(join(reader) == data, "the transfer must arrive intact");
    }

    /// A guest that reads slowly parks the pump while the event loop goes
    /// on, and still receives every byte.
    #[tokio::test]
    async fn a_slow_reader_parks_the_pump_without_blocking_the_loop() {
        let data = Rng::new(2).bytes(1 << 20);
        let (src, src_writer) = io::pipe().unwrap();
        let (dst_reader, dst) = io::pipe().unwrap();
        let writer = write_on_a_thread(src_writer, &data);
        let pump = tokio::spawn(pump(src.into(), dst.into()));

        // Nobody reads yet, so the pump fills the guest's pipe and waits for
        // it to drain. This timer fires on the same thread meanwhile.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!pump.is_finished());

        let reader = read_on_a_thread(dst_reader, 4096);
        pump.await.unwrap().unwrap();

        join(writer);
        assert!(join(reader) == data, "the transfer must arrive intact");
    }

    /// Two pumps may write into one pipe, as when a client passes the same
    /// pipe to two receives. Neither changes the flags of the client's open
    /// file description, so neither can leave the other's writes blocking.
    #[tokio::test]
    async fn pumps_into_one_pipe_never_change_its_flags() {
        use nix::fcntl::{fcntl, FcntlArg};
        let flags = |pipe: &io::PipeWriter| fcntl(pipe, FcntlArg::F_GETFL).unwrap();
        let (dst_reader, guest_pipe) = io::pipe().unwrap();
        let original = flags(&guest_pipe);
        let (first_src, first_writer) = io::pipe().unwrap();
        let (second_src, second_writer) = io::pipe().unwrap();
        let first_dst = guest_pipe.try_clone().unwrap();
        let second_dst = guest_pipe.try_clone().unwrap();
        let first = tokio::spawn(pump(first_src.into(), first_dst.into()));
        let second = tokio::spawn(pump(second_src.into(), second_dst.into()));
        let writers = [
            write_on_a_thread(first_writer, b"first"),
            write_on_a_thread(second_writer, &[0; 1 << 20]),
        ];

        // Nobody reads yet, so the second pump fills the pipe and waits.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!second.is_finished());
        assert_eq!(flags(&guest_pipe), original);

        let reader = read_on_a_thread(dst_reader, 4096);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(flags(&guest_pipe), original);
        drop(guest_pipe);
        for writer in writers {
            join(writer);
        }
        assert_eq!(join(reader).len(), b"first".len() + (1 << 20));
    }

    /// A pipe the proxy may not open again, such as one from a client running
    /// as another user, still receives the transfer: it is written with
    /// `RWF_NOWAIT`, through the descriptor the client sent. A kernel whose
    /// pipes refuse that flag reopens them, which this pipe forbids, so the
    /// test is skipped there.
    #[tokio::test]
    async fn pump_writes_into_a_pipe_it_cannot_reopen() {
        use nix::sys::stat::{fchmod, Mode};
        let (_probe_reader, probe) = io::pipe().unwrap();
        let probe = std::fs::File::from(std::os::fd::OwnedFd::from(probe));
        if write_nowait(&probe, b"?").is_err_and(|e| e.raw_os_error() == Some(libc::EOPNOTSUPP)) {
            eprintln!("skipped: this kernel's pipes refuse RWF_NOWAIT, so it reopens them");
            return;
        }
        let data = Rng::new(4).bytes(1 << 20);
        let (src, src_writer) = io::pipe().unwrap();
        let (dst_reader, dst) = io::pipe().unwrap();
        fchmod(&dst, Mode::S_IRUSR).unwrap();
        let writer = write_on_a_thread(src_writer, &data);
        let reader = read_on_a_thread(dst_reader, 4096);

        pump(src.into(), dst.into()).await.unwrap();

        join(writer);
        assert!(join(reader) == data, "the transfer must arrive intact");
    }

    /// A named FIFO refuses `RWF_NOWAIT`, so the pump reopens it and writes
    /// that private description.
    #[tokio::test]
    async fn pump_writes_into_a_named_fifo() {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        use std::os::unix::fs::OpenOptionsExt;
        let dir = crate::test_support::TempDir::new("clipboard-fifo");
        let path = dir.path().join("fifo");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
        // Opening the read end first lets the write end open at once.
        let fifo_reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        fcntl(&fifo_reader, FcntlArg::F_SETFL(OFlag::empty())).unwrap();
        let fifo = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let (src, src_writer) = io::pipe().unwrap();
        let writer = write_on_a_thread(src_writer, b"through a named FIFO");
        let reader = read_on_a_thread(fifo_reader, 4096);

        pump(src.into(), fifo.into()).await.unwrap();

        join(writer);
        assert_eq!(join(reader), b"through a named FIFO");
    }

    /// The fallback for kernels whose pipes refuse `RWF_NOWAIT` writes a
    /// private non-blocking description and leaves the guest's alone.
    #[test]
    fn a_reopened_pipe_is_private_and_non_blocking() {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        let flags = |file: &std::fs::File| {
            OFlag::from_bits_truncate(fcntl(file, FcntlArg::F_GETFL).unwrap())
        };
        let (mut reader, writer) = io::pipe().unwrap();
        let guest_pipe = std::fs::File::from(std::os::fd::OwnedFd::from(writer));
        let original = flags(&guest_pipe);

        let mut reopened = reopen(&guest_pipe).unwrap();

        assert!(flags(&reopened).contains(OFlag::O_NONBLOCK));
        assert_eq!(flags(&guest_pipe), original);
        reopened.write_all(b"private").unwrap();
        let mut received = [0; 7];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"private");
    }

    /// A guest that closes its pipe ends the transfer, whether before the
    /// pump starts or mid-transfer: the pump closes the host's pipe instead
    /// of waiting for the host to finish writing.
    #[tokio::test]
    async fn a_closed_reader_ends_the_pump() {
        use tokio::io::AsyncReadExt;
        for mid_transfer in [false, true] {
            let (src, mut src_writer) = io::pipe().unwrap();
            let (dst_reader, dst) = io::pipe().unwrap();
            let mut dst_reader =
                tokio::net::unix::pipe::Receiver::from_owned_fd(dst_reader.into()).unwrap();
            let pump = tokio::spawn(pump(src.into(), dst.into()));
            if mid_transfer {
                src_writer.write_all(b"wanted").unwrap();
                let mut wanted = [0; 6];
                dst_reader.read_exact(&mut wanted).await.unwrap();
            }
            drop(dst_reader);
            src_writer.write_all(b"unwanted").unwrap();

            pump.await.unwrap().unwrap();

            assert_eq!(
                src_writer.write(b"more").unwrap_err().kind(),
                io::ErrorKind::BrokenPipe,
                "mid_transfer={mid_transfer}: the pump must have closed the host's pipe"
            );
        }
    }

    /// A destination that is not a pipe, a socket or a regular file, such
    /// as a device whose write may sleep, is refused, and dropping it lets
    /// the guest read EOF.
    #[tokio::test]
    async fn pump_refuses_a_character_device() {
        let (src, _src_writer) = io::pipe().unwrap();
        let null = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();

        let error = pump(src.into(), null.into()).await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    /// A transfer that moves no data for `PUMP_IDLE_TIMEOUT` ends and closes
    /// both pipes; every byte that moves restarts the timeout.
    #[tokio::test(start_paused = true)]
    async fn a_transfer_the_host_stalls_ends_after_the_idle_timeout() {
        let (src, mut src_writer) = io::pipe().unwrap();
        let (mut dst_reader, dst) = io::pipe().unwrap();
        let start = tokio::time::Instant::now();
        let pump = tokio::spawn(pump(src.into(), dst.into()));

        src_writer.write_all(b"a").unwrap();
        tokio::time::sleep(PUMP_IDLE_TIMEOUT - Duration::from_secs(1)).await;
        src_writer.write_all(b"b").unwrap();
        let error = pump.await.unwrap().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= PUMP_IDLE_TIMEOUT * 2 - Duration::from_secs(1));
        let mut received = Vec::new();
        dst_reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"ab", "the guest reads what moved, then EOF");
        assert_eq!(
            src_writer.write(b"c").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe,
            "the pump must have closed the host's pipe"
        );
    }

    /// So does a transfer whose guest stops reading while its pipe is full.
    #[tokio::test(start_paused = true)]
    async fn a_transfer_the_guest_stalls_ends_after_the_idle_timeout() {
        use nix::fcntl::{fcntl, FcntlArg};
        let (src, mut src_writer) = io::pipe().unwrap();
        let (mut dst_reader, dst) = io::pipe().unwrap();
        // A one-page pipe fills long before the pump's buffer empties.
        let capacity = fcntl(&dst, FcntlArg::F_SETPIPE_SZ(4096)).unwrap();
        src_writer.write_all(&[0; 8192]).unwrap();
        let start = tokio::time::Instant::now();

        let error = pump(src.into(), dst.into()).await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= PUMP_IDLE_TIMEOUT);
        let mut received = Vec::new();
        dst_reader.read_to_end(&mut received).unwrap();
        assert_eq!(received.len(), capacity as usize, "the guest reads EOF");
    }

    /// A guest may receive into a regular file, such as a memfd, which the
    /// pump writes directly.
    #[tokio::test]
    async fn pump_writes_into_a_regular_file() {
        use std::io::{Seek, SeekFrom};
        let data = Rng::new(3).bytes(1 << 20);
        let (src, src_writer) = io::pipe().unwrap();
        let mut file = crate::test_support::shm_pool_file(&[]);
        let writer = write_on_a_thread(src_writer, &data);

        pump(src.into(), file.try_clone().unwrap().into())
            .await
            .unwrap();

        join(writer);
        let mut received = Vec::new();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_to_end(&mut received).unwrap();
        assert!(received == data, "the transfer must arrive intact");
    }

    #[test]
    fn host_data_offer_gets_a_guest_server_id_mapping() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        // Host wl_data_device object 80 is represented by guest object 10.
        ctx.shadow_table
            .insert_paired_for_test(10, 80, "wl_data_device");

        // wl_data_device.data_offer(new_id wl_data_offer) is a host-generated
        // object event. The host chooses its ID, which must not be forwarded
        // verbatim into the guest namespace.
        let host_offer_id: u32 = 0xff00_0123;
        let payload = host_offer_id.to_ne_bytes();
        let mut msg = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload);
        let mut handler = DataDeviceHandler;
        let result = wl_data_device::dispatch_event(&mut msg, &mut handler, &mut ctx)
            .expect("data_offer should decode")
            .expect("data_offer should be forwarded");

        let (wire, fds) = result;
        assert!(fds.is_empty());
        assert_eq!(
            crate::test_support::msg_sender(&wire),
            10,
            "event sender must be translated to the guest object"
        );
        let guest_offer_id = u32::from_ne_bytes(wire[8..12].try_into().unwrap());
        assert_ne!(guest_offer_id, host_offer_id);
        assert!(
            guest_offer_id >= 0xff00_0000,
            "server-generated guest IDs must use Wayland's reserved range"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_offer_id),
            Some(host_offer_id)
        );
        assert_eq!(
            ctx.shadow_table.get_interface(guest_offer_id),
            Some("wl_data_offer")
        );
    }

    #[test]
    fn host_data_offer_rejects_reused_host_object_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_for_test(10, 80, "wl_data_device");
        let host_surface = 0xff00_0123;
        ctx.shadow_table
            .insert_host_only_for_test(host_surface, "wl_surface", 6);

        let payload = host_surface.to_ne_bytes();
        let mut msg = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload);
        let mut handler = DataDeviceHandler;
        let result = wl_data_device::dispatch_event(&mut msg, &mut handler, &mut ctx);

        assert_eq!(
            raw_dispatch(result),
            Err(crate::wire::ProtocolError::InvalidObjectId(host_surface))
        );
        assert!(
            ctx.shadow_table.get_guest_id(host_surface).is_none(),
            "a colliding host ID must not overwrite an existing mapping"
        );
    }

    /// libwayland-server creates objects only with server IDs. One from the
    /// client range is the proxy's to allocate, so taking it would let the
    /// proxy's next object collide with it.
    #[test]
    fn host_data_offer_rejects_a_client_range_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_for_test(10, 80, "wl_data_device");

        let payload = 123u32.to_ne_bytes();
        let mut msg = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload);
        let result = wl_data_device::dispatch_event(&mut msg, &mut DataDeviceHandler, &mut ctx);

        assert_eq!(
            raw_dispatch(result),
            Err(crate::wire::ProtocolError::InvalidObjectId(123))
        );
        assert!(ctx.shadow_table.is_host_id_available(123));
    }

    #[test]
    fn event_from_an_unmapped_sender_maps_no_new_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        // The data device object exists, but this host ID was never mapped.
        ctx.shadow_table
            .insert_host_only_for_test(81, "wl_data_device", 3);
        let host_offer: u32 = 0xff00_0001;
        let payload = host_offer.to_ne_bytes();
        let mut offer = WireMessage::new(81, wl_data_device::EVT_DATA_OFFER, &payload);

        assert_eq!(
            raw_dispatch(wl_data_device::dispatch_event(
                &mut offer,
                &mut DataDeviceHandler,
                &mut ctx
            )),
            Ok(None)
        );
        assert_eq!(ctx.shadow_table.get_guest_id(host_offer), None);
        assert_eq!(
            ctx.shadow_table
                .adopt_host_created(0xff00_0009, "wl_data_offer", 3),
            0xff00_0000,
            "a dropped event must not consume a guest server ID"
        );
    }

    /// Generated dispatch checks every object argument before it maps a
    /// `new_id`, so a request dropped for naming a local object where the
    /// host needs one of its own maps nothing and uses up no host ID:
    /// libwayland-server refuses the next new ID after a gap. This pins the
    /// generated dispatcher, with a handler that forwards: `OutputHandler`
    /// makes the xdg_output of an inert output inert instead
    /// (`xdg_output_of_an_inert_output_is_inert`).
    #[test]
    fn request_dropped_for_a_local_argument_maps_no_new_id() {
        use crate::protocols::xdg_output_unstable_v1::zxdg_output_manager_v1::{
            self, ZxdgOutputManagerV1Handler,
        };
        struct Forwarding;
        impl ZxdgOutputManagerV1Handler for Forwarding {}
        const MANAGER: u32 = 10;
        const INERT_OUTPUT: u32 = 11;
        const XDG_OUTPUT: u32 = 12;
        let context = || {
            let mut ctx = Context::new_for_test(false, false, Vec::new());
            ctx.shadow_table
                .insert_paired_for_test(MANAGER, 20, "zxdg_output_manager_v1");
            ctx.shadow_table.create_local(INERT_OUTPUT, "wl_output", 4);
            ctx
        };
        // The host ID the next object takes when nothing used one up.
        let next_host_id = context()
            .shadow_table
            .create_paired(13, "zxdg_output_v1", 3, 3);

        let mut ctx = context();
        let payload: Vec<u8> = [XDG_OUTPUT, INERT_OUTPUT]
            .iter()
            .flat_map(|id| id.to_ne_bytes())
            .collect();
        let mut get = WireMessage::new(
            MANAGER,
            zxdg_output_manager_v1::REQ_GET_XDG_OUTPUT,
            &payload,
        );
        let result = zxdg_output_manager_v1::dispatch_request(&mut get, &mut Forwarding, &mut ctx);

        assert_eq!(raw_dispatch(result), Ok(None));
        assert!(
            ctx.shadow_table.is_guest_id_available(XDG_OUTPUT),
            "a dropped request must not map its new_id"
        );
        assert_eq!(
            ctx.shadow_table.create_paired(13, "zxdg_output_v1", 3, 3),
            next_host_id,
            "a dropped request must not use up a host ID"
        );
    }

    /// Every clipboard or drag-and-drop selection brings a new host data
    /// offer. The guest IDs must be recycled, or a long session would run
    /// out of server IDs.
    #[test]
    fn cycled_data_offers_keep_their_guest_id_small() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 80, "wl_data_device", 3);
        let mut handler = DataDeviceHandler;
        let mut first_guest_offer = None;

        for round in 0..1000u32 {
            // The host may pick any free server ID for each offer.
            let host_offer = 0xff00_0000 + round % 3;
            let payload = host_offer.to_ne_bytes();
            let mut offer = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload);
            let (event, _) = wl_data_device::dispatch_event(&mut offer, &mut handler, &mut ctx)
                .expect("data_offer should decode")
                .expect("data_offer should be forwarded");
            let guest_offer = u32::from_ne_bytes(event[8..12].try_into().unwrap());
            assert_eq!(
                guest_offer,
                *first_guest_offer.get_or_insert(guest_offer),
                "round {round}: a destroyed offer's guest ID must be reused"
            );

            let mut destroy = WireMessage::new(
                guest_offer,
                crate::protocols::wayland::wl_data_offer::REQ_DESTROY,
                &[],
            );
            crate::protocols::wayland::wl_data_offer::dispatch_request(
                &mut destroy,
                &mut handler,
                &mut ctx,
            )
            .expect("data_offer.destroy should decode");
        }
    }

    #[test]
    fn destroyed_host_data_offer_allows_ordered_server_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 80, "wl_data_device", 3);
        let host_offer_id = 0xff00_0000u32;
        let mut handler = DataDeviceHandler;

        let payload = host_offer_id.to_ne_bytes();
        let mut first_offer = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload);
        let (first_event, _) =
            wl_data_device::dispatch_event(&mut first_offer, &mut handler, &mut ctx)
                .expect("first data_offer should decode")
                .expect("first data_offer should be forwarded");
        let first_guest_id = u32::from_ne_bytes(first_event[8..12].try_into().unwrap());
        assert_eq!(
            ctx.shadow_table.get_host_id(first_guest_id),
            Some(host_offer_id)
        );

        let mut destroy = WireMessage::new(
            first_guest_id,
            crate::protocols::wayland::wl_data_offer::REQ_DESTROY,
            &[],
        );
        let (destroy_request, _) = crate::protocols::wayland::wl_data_offer::dispatch_request(
            &mut destroy,
            &mut handler,
            &mut ctx,
        )
        .expect("data_offer.destroy should decode")
        .expect("data_offer.destroy should be forwarded");
        assert_eq!(
            crate::test_support::msg_sender(&destroy_request),
            host_offer_id
        );
        assert_eq!(ctx.shadow_table.get_host_id(first_guest_id), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_offer_id), None);
        assert!(!ctx.shadow_table.is_pending_destroy_guest(first_guest_id));

        let mut replacement = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload);
        let (replacement_event, _) =
            wl_data_device::dispatch_event(&mut replacement, &mut handler, &mut ctx)
                .expect("reused server ID should decode")
                .expect("replacement data_offer should be forwarded");
        let replacement_guest_id = u32::from_ne_bytes(replacement_event[8..12].try_into().unwrap());
        assert_eq!(
            replacement_guest_id, first_guest_id,
            "a destroyed server-created object's guest ID is reused first"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(replacement_guest_id),
            Some(host_offer_id)
        );
    }
}
