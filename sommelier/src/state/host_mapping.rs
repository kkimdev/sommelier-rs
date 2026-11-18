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
    /// Map `len` bytes of `fd` from `offset`, shared and read-write.
    pub fn map_shared(fd: BorrowedFd<'_>, len: usize, offset: libc::off_t) -> io::Result<Self> {
        Self::map(fd, len, offset, libc::PROT_READ | libc::PROT_WRITE)
    }

    /// Map `len` bytes of `fd`, shared and read-only, so that writing the
    /// mapping faults.
    #[cfg(test)]
    pub fn map_read_only(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        Self::map(fd, len, 0, libc::PROT_READ)
    }

    fn map(
        fd: BorrowedFd<'_>,
        len: usize,
        offset: libc::off_t,
        prot: libc::c_int,
    ) -> io::Result<Self> {
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
    /// before the range is full fails with `UnexpectedEof`.
    pub fn pread_into(&self, dst: Range<usize>, file: &File, mut offset: u64) -> io::Result<()> {
        if dst.start > dst.end || dst.end > self.len {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        let mut start = dst.start;
        while start < dst.end {
            let file_offset = libc::off_t::try_from(offset)
                .map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
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

    /// A copy of the mapped bytes.
    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<u8> {
        let mut bytes = vec![0; self.len];
        // SAFETY: the mapping is readable for `len` bytes, and `bytes` is a
        // distinct allocation of that length.
        unsafe { ptr::copy_nonoverlapping(self.ptr.as_ptr(), bytes.as_mut_ptr(), self.len) };
        bytes
    }
}

impl Drop for HostMapping {
    fn drop(&mut self) {
        // SAFETY: the value owns the mapping, and nothing uses it after
        // drop.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::HostMapping;
    use crate::test_support::shm_pool_file;
    use std::os::fd::AsFd;

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

    #[test]
    fn pread_into_past_the_end_of_the_file_is_an_eof() {
        let mapping = zeroed(4);
        let error = mapping
            .pread_into(0..4, &shm_pool_file(&[0x11; 2]), 0)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_failed_mmap_is_an_error() {
        let file = shm_pool_file(&[0; 4]);
        assert!(HostMapping::map_shared(file.as_fd(), 0, 0).is_err());
    }
}
