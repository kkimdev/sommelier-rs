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

//! The proxy's mapping of a buffer's host copy.

use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::{self, NonNull};

/// A shared mapping of a buffer's host copy, which the proxy writes only by
/// reading pool pixels into it with `pread`.
///
/// The host compositor maps the same memory and may sample it at any time,
/// and the mapping can stop being writable under the proxy (a dma-buf the
/// driver revokes, or a read-only mapping in a test). So no Rust reference
/// to the mapped bytes is ever made: the kernel writes them, through a raw
/// pointer, in [`HostMapping::pread_into`], and reports a fault as `EFAULT`
/// instead of raising a signal. A dma-buf host copy is written only inside
/// the VirtWL dma-buf CPU-write interval, which the caller opens.
///
/// The mapping is made by a successful `mmap` and unmapped on drop, so the
/// pointer is valid for `len` bytes for as long as the value lives.
pub struct HostMapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the value owns its mapping, which any thread may use or unmap.
// Its methods write the mapped bytes only through the kernel and make no
// Rust reference to them, so concurrent use can tear bytes, as the host's
// concurrent reads can, but cannot alias a reference.
unsafe impl Send for HostMapping {}
unsafe impl Sync for HostMapping {}

impl HostMapping {
    /// Map `len` bytes of `fd` from `offset`, shared and read-write. An
    /// empty mapping is refused with `EINVAL`.
    pub fn map_shared(fd: BorrowedFd<'_>, len: usize, offset: u64) -> io::Result<Self> {
        Self::map(fd, len, offset, libc::PROT_READ | libc::PROT_WRITE)
    }

    /// Map `len` bytes of `fd`, shared and read-only, so that writing the
    /// mapping faults.
    #[cfg(test)]
    pub fn map_read_only(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        Self::map(fd, len, 0, libc::PROT_READ)
    }

    fn map(fd: BorrowedFd<'_>, len: usize, offset: u64, prot: libc::c_int) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let offset = file_offset(offset)?;
        #[cfg(test)]
        crate::test_support::counting::count_host_map();
        // SAFETY: a new mapping, at an address the kernel chooses, aliases
        // no memory the program already uses.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { ptr, len })
    }

    /// The length of the mapping in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Fill `dst` of the mapping with the bytes of `file` from `offset`.
    ///
    /// A range that does not lie inside the mapping fails with `EFAULT`,
    /// as a fault in the mapping does, and writes nothing. A file that ends
    /// before the range is full fails with `UnexpectedEof`, and an offset
    /// past `off_t` with `EOVERFLOW`. A fault, the end of the file or an
    /// I/O error part way through leaves a prefix of the range written.
    pub fn pread_into(&self, dst: Range<usize>, file: &File, mut offset: u64) -> io::Result<()> {
        if dst.start > dst.end || dst.end > self.len {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        let mut start = dst.start;
        while start < dst.end {
            let file_offset = file_offset(offset)?;
            #[cfg(test)]
            crate::test_support::counting::count_pool_read();
            // SAFETY: `start..dst.end` lies inside the mapping, which lives
            // as long as `self`. Only the kernel writes through the pointer.
            let read = unsafe {
                libc::pread(
                    file.as_raw_fd(),
                    self.ptr.as_ptr().add(start).cast(),
                    dst.end - start,
                    file_offset,
                )
            };
            match read {
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the file ended before the range was full",
                    ))
                }
                read if read < 0 => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                read => {
                    start += read as usize;
                    offset += read as u64;
                }
            }
        }
        Ok(())
    }

    /// A copy of the mapped bytes, which the kernel reads from the mapping
    /// into a memfd, as it writes the mapping in `pread_into`.
    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<u8> {
        use std::os::unix::fs::FileExt;
        let copy = crate::test_support::shm_pool_file(&[]);
        // SAFETY: the mapping, which lives as long as `self`, is readable
        // for `len` bytes. Only the kernel reads through the pointer.
        let written =
            unsafe { libc::pwrite(copy.as_raw_fd(), self.ptr.as_ptr().cast(), self.len, 0) };
        assert_eq!(
            usize::try_from(written).ok(),
            Some(self.len),
            "copy the mapping into a memfd: {}",
            io::Error::last_os_error()
        );
        let mut bytes = vec![0; self.len];
        copy.read_exact_at(&mut bytes, 0)
            .expect("read the memfd back");
        bytes
    }
}

impl Drop for HostMapping {
    fn drop(&mut self) {
        // SAFETY: the value owns the mapping, of a nonzero `len`, and
        // nothing uses it after drop.
        let unmapped = unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
        debug_assert_eq!(unmapped, 0, "munmap of an owned mapping failed");
    }
}

/// `offset` as a file offset for `mmap` and `pread`, or `EOVERFLOW`.
fn file_offset(offset: u64) -> io::Result<libc::off_t> {
    libc::off_t::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
}

#[cfg(test)]
mod tests {
    use super::HostMapping;
    use crate::test_support::shm_pool_file;
    use std::ops::Range;
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;

    // The EINTR retry in `pread_into` cannot practically be unit-tested: it
    // needs a signal to land during a `pread` of a memfd. A short read
    // before the end of a regular file happens only at a fault, which
    // `a_fault_part_way_leaves_the_prefix_written` provokes.

    /// A mapping of `len` zero bytes.
    fn zeroed(len: usize) -> HostMapping {
        HostMapping::map_shared(shm_pool_file(&vec![0; len]).as_fd(), len, 0).unwrap()
    }

    #[test]
    fn pread_into_fills_only_its_range() {
        let mapping = zeroed(4);
        mapping
            .pread_into(1..3, &shm_pool_file(&[1, 2, 3]), 1)
            .unwrap();
        assert_eq!(mapping.to_vec(), [0, 2, 3, 0]);
    }

    /// The mapping is shorter than a page, so a range that ends a byte past
    /// it would still be writable memory: only the bounds check refuses it.
    #[test]
    fn pread_into_refuses_a_range_past_the_mapping() {
        let mapping = zeroed(4);
        let source = shm_pool_file(&[0x11; 8]);
        for range in [0..5, 4..5] {
            let error = mapping.pread_into(range.clone(), &source, 0).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EFAULT), "{range:?}");
        }
        assert_eq!(mapping.to_vec(), [0; 4], "nothing is written");
        mapping.pread_into(4..4, &source, 0).unwrap();
    }

    /// A reversed range, and an empty one past the end, are refused like
    /// any range outside the mapping.
    #[test]
    fn pread_into_refuses_reversed_and_empty_ranges_outside_the_mapping() {
        let mapping = zeroed(4);
        let source = shm_pool_file(&[0x11; 8]);
        for range in [Range { start: 3, end: 2 }, 5..5] {
            let error = mapping.pread_into(range.clone(), &source, 0).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EFAULT), "{range:?}");
        }
        assert_eq!(mapping.to_vec(), [0; 4], "nothing is written");
    }

    #[test]
    fn pread_into_refuses_an_offset_past_off_t() {
        let error = zeroed(4)
            .pread_into(0..4, &shm_pool_file(&[0x11; 4]), u64::MAX)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EOVERFLOW));
    }

    /// The kernel writes the file the mapping shares, not a private copy:
    /// the host reads the host copy through its own descriptor.
    #[test]
    fn pread_into_writes_the_shared_file() {
        let file = shm_pool_file(&[0; 4]);
        let mapping = HostMapping::map_shared(file.as_fd(), 4, 0).unwrap();
        mapping
            .pread_into(0..4, &shm_pool_file(&[1, 2, 3, 4]), 0)
            .unwrap();
        let mut bytes = [0; 4];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, [1, 2, 3, 4]);
    }

    fn page_size() -> usize {
        // SAFETY: sysconf has no preconditions.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap()
    }

    /// A mapping whose second page lies past the end of its file faults
    /// there. The kernel reports the fault as `EFAULT` instead of raising
    /// SIGBUS, so the process survives.
    #[test]
    fn a_fault_past_the_end_of_the_file_is_an_efault() {
        let page = page_size();
        let file = shm_pool_file(&vec![0; page]);
        let mapping = HostMapping::map_shared(file.as_fd(), 2 * page, 0).unwrap();
        let error = mapping
            .pread_into(page..page + 4, &shm_pool_file(&[0x11; 4]), 0)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EFAULT));
    }

    #[test]
    fn a_fault_part_way_leaves_the_prefix_written() {
        let page = page_size();
        let file = shm_pool_file(&vec![0; page]);
        let mapping = HostMapping::map_shared(file.as_fd(), 2 * page, 0).unwrap();
        let error = mapping
            .pread_into(page - 4..page + 4, &shm_pool_file(&[0x11; 8]), 0)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EFAULT));
        let mut prefix = [0; 4];
        file.read_exact_at(&mut prefix, (page - 4) as u64).unwrap();
        assert_eq!(prefix, [0x11; 4], "the bytes before the fault are written");
    }

    #[test]
    fn pread_into_past_the_end_of_the_file_is_an_eof() {
        let mapping = zeroed(4);
        let error = mapping
            .pread_into(0..4, &shm_pool_file(&[0x11; 2]), 0)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_zero_length_mapping_is_refused() {
        let file = shm_pool_file(&[0; 4]);
        let error = HostMapping::map_shared(file.as_fd(), 0, 0).err().unwrap();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn an_unaligned_offset_is_refused() {
        let file = shm_pool_file(&[0; 8]);
        assert!(HostMapping::map_shared(file.as_fd(), 4, 1).is_err());
    }
}
