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
use std::mem::MaybeUninit;
use std::ops::Range;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::{self, NonNull};

/// The most buffers one `preadv` takes: Linux's `IOV_MAX`.
const MAX_IOVECS: usize = 1024;

/// The longest run of file bytes between two rows that a batched read
/// reads into scratch memory, to read many rows in one system call; rows
/// further apart are read one at a time. Measured on a Crostini guest
/// over 900 rows of 8 to 4096 bytes from a memfd, batches took a third to
/// a half of the per-row time with 1 KiB gaps, broke even around 2 to
/// 4 KiB, and took several times longer from 8 KiB on.
const MAX_READ_GAP: usize = 1024;

/// Rows of a file to read into a [`HostMapping`]: `rows` rows of
/// `row_bytes`, the first at `src_offset` in the file and `dst_start` in
/// the mapping, each next one `src_stride` and `dst_stride` further on.
#[derive(Clone, Copy, Debug)]
pub struct Rows {
    pub src_offset: u64,
    pub src_stride: usize,
    pub dst_start: usize,
    pub dst_stride: usize,
    pub row_bytes: usize,
    pub rows: usize,
}

impl Rows {
    /// The mapping range of row `row`.
    fn dst(&self, row: usize) -> Range<usize> {
        let start = self.dst_start + row * self.dst_stride;
        start..start + self.row_bytes
    }

    /// The file offset of row `row`.
    fn src(&self, row: usize) -> u64 {
        self.src_offset + (row * self.src_stride) as u64
    }
}

/// A shared mapping of a buffer's host copy, which the proxy writes only by
/// reading pool pixels into it with `pread`.
///
/// The host compositor maps the same memory and may sample it at any time,
/// and the mapping can stop being writable under the proxy (a dma-buf the
/// driver revokes, or a read-only mapping in a test). So no Rust reference
/// to the mapped bytes is ever made: the kernel writes them, through a raw
/// pointer, in [`HostMapping::pread_into`] and
/// [`HostMapping::pread_rows_into`], and reports a fault as `EFAULT`
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

    /// Fill the mapping's `rows` with the file's, and nothing else of the
    /// mapping, in as few system calls as is cheap.
    ///
    /// Rows that are contiguous in both are one `pread`. Otherwise, while
    /// the file's rows lie at most [`MAX_READ_GAP`] bytes apart, one
    /// `preadv` reads up to 512 rows (1024 when the file's rows touch),
    /// reading each gap between them into scratch memory, never into the
    /// mapping. Rows further apart are read one `pread` each. Errors are
    /// those of [`Self::pread_into`]: rows outside the mapping fail with
    /// `EFAULT` before anything is written, and a fault, the end of the
    /// file or an I/O error part way through leaves the rows before it
    /// written.
    pub fn pread_rows_into(&self, rows: Rows, file: &File) -> io::Result<()> {
        if rows.rows == 0 || rows.row_bytes == 0 {
            return Ok(());
        }
        let last_end = (rows.rows - 1)
            .checked_mul(rows.dst_stride)
            .and_then(|start| start.checked_add(rows.dst_start))
            .and_then(|start| start.checked_add(rows.row_bytes));
        let last_src = (rows.rows - 1)
            .checked_mul(rows.src_stride)
            .and_then(|start| u64::try_from(start).ok())
            .and_then(|start| start.checked_add(rows.src_offset));
        if last_end.is_none_or(|end| end > self.len) {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        if last_src.is_none() {
            return Err(io::Error::from_raw_os_error(libc::EOVERFLOW));
        }
        if rows.src_stride == rows.row_bytes && rows.dst_stride == rows.row_bytes {
            return self.pread_into(
                rows.dst_start..rows.dst_start + rows.rows * rows.row_bytes,
                file,
                rows.src_offset,
            );
        }
        match rows.src_stride.checked_sub(rows.row_bytes) {
            Some(gap) if gap <= MAX_READ_GAP => self.preadv_rows_into(rows, gap, file),
            _ => (0..rows.rows)
                .try_for_each(|row| self.pread_into(rows.dst(row), file, rows.src(row))),
        }
    }

    /// [`Self::pread_rows_into`] in batches of rows that one `preadv` each
    /// reads, with the `gap` bytes between two rows read into scratch
    /// memory. The buffers and the scratch live on the stack, so the copy
    /// allocates nothing.
    fn preadv_rows_into(&self, rows: Rows, gap: usize, file: &File) -> io::Result<()> {
        // Only the kernel writes either array, and nothing reads the
        // scratch; the buffers the call passes are initialized first.
        let mut scratch = [MaybeUninit::<u8>::uninit(); MAX_READ_GAP];
        let mut iovecs = [MaybeUninit::<libc::iovec>::uninit(); MAX_IOVECS];
        let rows_per_batch = if gap == 0 {
            MAX_IOVECS
        } else {
            MAX_IOVECS.div_ceil(2)
        };
        for first in (0..rows.rows).step_by(rows_per_batch) {
            let batch = first..(first + rows_per_batch).min(rows.rows);
            let mut count = 0;
            let mut total = 0;
            for row in batch.clone() {
                if row != first && gap != 0 {
                    iovecs[count].write(libc::iovec {
                        iov_base: scratch.as_mut_ptr().cast(),
                        iov_len: gap,
                    });
                    count += 1;
                }
                iovecs[count].write(libc::iovec {
                    // SAFETY: `pread_rows_into` checked that every row lies
                    // inside the mapping, which lives as long as `self`.
                    iov_base: unsafe { self.ptr.as_ptr().add(rows.dst(row).start) }.cast(),
                    iov_len: rows.row_bytes,
                });
                count += 1;
                total += rows.row_bytes + if row != first { gap } else { 0 };
            }
            let offset = file_offset(rows.src(first))?;
            let read = loop {
                #[cfg(test)]
                crate::test_support::counting::count_pool_read();
                // SAFETY: the first `count` buffers are initialized, and
                // each is a row inside the mapping or the scratch, which
                // outlive the call. Only the kernel writes through them.
                let read = unsafe {
                    libc::preadv(
                        file.as_raw_fd(),
                        iovecs.as_ptr().cast(),
                        count as libc::c_int,
                        offset,
                    )
                };
                if read >= 0 {
                    break read as usize;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            };
            if read < total {
                // A fault, the end of the file or a signal stopped the read
                // part way. Read the rest of the batch's rows one at a
                // time, which goes on after a signal and reports the rest.
                let done = rows.src(first) + read as u64;
                for row in batch {
                    let src = rows.src(row);
                    let skip = done.saturating_sub(src).min(rows.row_bytes as u64) as usize;
                    let dst = rows.dst(row);
                    self.pread_into(dst.start + skip..dst.end, file, src + skip as u64)?;
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
    use super::{HostMapping, Rows, MAX_READ_GAP};
    use crate::test_support::counting::Counts;
    use crate::test_support::{shm_pool_file, Rng};
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

    /// What `rows` of `file` leave in a mapping of `len` bytes of `0xee`,
    /// read by `pread_rows_into`, and how many reads that took; or what one
    /// `pread_into` per row leaves there.
    fn read_rows(file: &std::fs::File, len: usize, rows: Rows, batched: bool) -> (Vec<u8>, u64) {
        let mapping =
            HostMapping::map_shared(shm_pool_file(&vec![0xee; len]).as_fd(), len, 0).unwrap();
        let (cost, ()) = Counts::of(|| {
            if batched {
                mapping.pread_rows_into(rows, file).unwrap();
            } else {
                for row in 0..rows.rows {
                    mapping
                        .pread_into(rows.dst(row), file, rows.src(row))
                        .unwrap();
                }
            }
        });
        (mapping.to_vec(), cost.pool_reads)
    }

    /// Batched reads write what one read per row does, and nothing else:
    /// whatever the strides, the gaps between the file's rows and the
    /// number of rows, across batches.
    #[test]
    fn pread_rows_into_writes_exactly_the_rows() {
        let mut rng = Rng::new(7);
        for case in 0..200 {
            let row_bytes = 1 + rng.below(64);
            let small_gap = 1 + rng.below(64);
            let gap = *rng.pick(&[0, small_gap, MAX_READ_GAP, MAX_READ_GAP + 1]);
            let padding = rng.below(32);
            let dst_padding = *rng.pick(&[0, 0, padding]);
            let many = rng.one_in(8);
            let rows = Rows {
                src_offset: rng.below(16) as u64,
                src_stride: row_bytes + gap,
                dst_start: rng.below(16),
                dst_stride: row_bytes + dst_padding,
                row_bytes,
                rows: 1 + rng.below(if many { 1500 } else { 40 }),
            };
            let src_len = rows.src(rows.rows - 1) as usize + row_bytes;
            let file = shm_pool_file(&rng.bytes(src_len));
            let len = rows.dst(rows.rows - 1).end + rng.below(16);
            let (batched, _) = read_rows(&file, len, rows, true);
            let (per_row, _) = read_rows(&file, len, rows, false);
            assert!(batched == per_row, "case {case}: {rows:?}");
        }
    }

    /// One read fills rows that touch in both the file and the mapping;
    /// rows the file keeps a small gap apart take one read per 512 rows,
    /// and those that touch in the file only one per 1024. Rows further
    /// apart take a read each.
    #[test]
    fn pread_rows_into_reads_many_rows_at_once() {
        let rows = |src_stride, dst_stride, rows| Rows {
            src_offset: 0,
            src_stride,
            dst_start: 0,
            dst_stride,
            row_bytes: 8,
            rows,
        };
        for (rows, reads) in [
            (rows(8, 8, 2000), 1),
            (rows(8, 16, 1024), 1),
            (rows(8, 16, 1025), 2),
            (rows(16, 16, 512), 1),
            (rows(16, 8, 513), 2),
            (rows(8 + MAX_READ_GAP, 8, 3), 1),
            (rows(9 + MAX_READ_GAP, 8, 3), 3),
        ] {
            let file = shm_pool_file(&vec![0x11; rows.src(rows.rows - 1) as usize + 8]);
            let len = rows.dst(rows.rows - 1).end;
            assert_eq!(read_rows(&file, len, rows, true).1, reads, "{rows:?}");
        }
    }

    /// Rows past the mapping are refused before anything is written; a
    /// file that ends part way leaves the rows before its end written.
    #[test]
    fn pread_rows_into_fails_like_pread_into() {
        let rows = Rows {
            src_offset: 0,
            src_stride: 8,
            dst_start: 0,
            dst_stride: 4,
            row_bytes: 4,
            rows: 3,
        };
        let mapping = zeroed(11);
        let file = shm_pool_file(&[0x11; 24]);
        let error = mapping.pread_rows_into(rows, &file).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EFAULT));
        assert_eq!(mapping.to_vec(), [0; 11], "nothing is written");

        let mapping = zeroed(12);
        let error = mapping
            .pread_rows_into(rows, &shm_pool_file(&[0x11; 18]))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(mapping.to_vec(), [&[0x11; 10][..], &[0; 2]].concat());
    }
}
