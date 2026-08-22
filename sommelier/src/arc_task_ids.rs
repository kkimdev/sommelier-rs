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

//! Process-shared allocation of fabricated ARC task-form application IDs.
//!
//! ChromeOS does not expose a Wayland request that asks the host for an
//! available ARC task ID.  The opt-in placement workaround therefore reserves
//! one high numeric block per Sommelier process with a filesystem `flock`.
//! Every window in that process consumes IDs sequentially from the block.
//! The lock is released automatically when the process closes its descriptor;
//! the lock files themselves intentionally remain so that deleting and
//! recreating a pathname can never split two locks across different inodes.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use log::info;

/// Lower bound of the private best-effort task-form ID pool.
pub(crate) const ARC_TASK_ID_POOL_START: u32 = 2_000_000_000;
/// Upper bound of the pool; `INT_MAX` remains the PR #2 compatibility sentinel.
pub(crate) const ARC_TASK_ID_POOL_END: u32 = i32::MAX as u32 - 1;
/// Number of task IDs reserved by one process block.
pub(crate) const ARC_TASK_ID_BLOCK_SIZE: u32 = 1_000_000;
const ARC_TASK_ID_BLOCK_COUNT: u32 =
    (ARC_TASK_ID_POOL_END - ARC_TASK_ID_POOL_START) / ARC_TASK_ID_BLOCK_SIZE + 1;

#[derive(Debug)]
struct ArcTaskIdBlock {
    /// The open descriptor keeps the block lock alive for the process.
    lock_file: Option<File>,
    start_id: u32,
    next_id: u32,
    end_id: u32,
}

/// Shared allocator handle passed to every client connection in one process.
///
/// The block lock is acquired before the proxy accepts clients.  The mutex
/// only protects the in-process cursor; the file lock coordinates independent
/// Sommelier processes.
#[derive(Debug)]
pub(crate) struct ArcTaskIdAllocator {
    block: Mutex<ArcTaskIdBlock>,
}

impl ArcTaskIdAllocator {
    /// Reserve one block in the process-shared runtime directory.
    ///
    /// # Returns
    ///
    /// An `Arc`-wrapped allocator whose descriptor remains locked until every
    /// clone is dropped.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if `XDG_RUNTIME_DIR` is unavailable, the lock
    /// directory cannot be created, or every block is currently occupied.
    pub(crate) fn acquire() -> io::Result<Arc<Self>> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "XDG_RUNTIME_DIR is required for ARC task ID block locks",
            )
        })?;
        let block_dir = PathBuf::from(runtime_dir)
            .join("sommelier")
            .join("arc-task-blocks");
        fs::create_dir_all(&block_dir)?;
        fs::set_permissions(&block_dir, Permissions::from_mode(0o700))?;

        let start_index = random_block_index()?;
        Self::acquire_in_directory(&block_dir, start_index)
    }

    /// Reserve a block beginning at a deterministic index.
    ///
    /// This helper is shared with tests so they can exercise lock contention
    /// without changing the production random-selection path.
    fn acquire_in_directory(directory: &Path, start_index: u32) -> io::Result<Arc<Self>> {
        for offset in 0..ARC_TASK_ID_BLOCK_COUNT {
            // `start_index` comes from `/dev/urandom`; use wrapping addition
            // before reducing into the small block-index domain so a debug
            // build cannot panic when the random word is near `u32::MAX`.
            let index = start_index.wrapping_add(offset) % ARC_TASK_ID_BLOCK_COUNT;
            let (start_id, end_id) = block_bounds(index);
            let path = directory.join(format!("{start_id}-{end_id}.lock"));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            options.mode(0o600);
            options.custom_flags(libc::O_CLOEXEC);
            let file = options.open(&path)?;

            match try_lock(&file) {
                Ok(()) => {
                    info!(
                        "Reserved ARC task ID block {}-{} using {}",
                        start_id,
                        end_id,
                        path.display()
                    );
                    return Ok(Arc::new(Self {
                        block: Mutex::new(ArcTaskIdBlock {
                            lock_file: Some(file),
                            start_id,
                            next_id: start_id,
                            end_id,
                        }),
                    }));
                }
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
                    ) =>
                {
                    // Another Sommelier owns this block.  The descriptor is
                    // dropped here, which leaves the other process's lock
                    // untouched.
                }
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "all ARC task ID blocks are already reserved",
        ))
    }

    /// Allocate the next task ID from this process's reserved block.
    ///
    /// # Errors
    ///
    /// Returns an error if the allocator mutex is poisoned or the block has
    /// no IDs left.
    pub(crate) fn allocate(&self) -> io::Result<u32> {
        let mut block = self
            .block
            .lock()
            .map_err(|_| io::Error::other("ARC task ID allocator mutex poisoned"))?;
        if block.next_id > block.end_id {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "ARC task ID block is exhausted",
            ));
        }

        let id = block.next_id;
        debug_assert!(id >= block.start_id);
        block.next_id = block.next_id.saturating_add(1);
        Ok(id)
    }

    /// Return the numeric range reserved by this process.
    #[cfg(test)]
    fn block_range(&self) -> (u32, u32) {
        let block = self.block.lock().expect("test allocator mutex");
        (block.start_id, block.end_id)
    }

    /// Build an in-memory allocator for placement unit tests.
    #[cfg(test)]
    pub(crate) fn for_test(start_id: u32, end_id: u32) -> Arc<Self> {
        assert!(start_id <= end_id);
        Arc::new(Self {
            block: Mutex::new(ArcTaskIdBlock {
                lock_file: None,
                start_id,
                next_id: start_id,
                end_id,
            }),
        })
    }
}

impl Drop for ArcTaskIdBlock {
    fn drop(&mut self) {
        // Keeping this field explicit documents the lifetime invariant: the
        // descriptor is closed only when the last shared allocator is gone.
        let _ = self.lock_file.take();
    }
}

fn try_lock(file: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn block_bounds(index: u32) -> (u32, u32) {
    debug_assert!(index < ARC_TASK_ID_BLOCK_COUNT);
    let start_id = ARC_TASK_ID_POOL_START + index * ARC_TASK_ID_BLOCK_SIZE;
    let end_id = start_id
        .saturating_add(ARC_TASK_ID_BLOCK_SIZE - 1)
        .min(ARC_TASK_ID_POOL_END);
    (start_id, end_id)
}

fn random_block_index() -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u32::from_ne_bytes(bytes) % ARC_TASK_ID_BLOCK_COUNT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock must be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sommelier-arc-task-block-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique test directory");
        path
    }

    #[test]
    fn block_bounds_keep_int_max_out_of_the_pool() {
        assert_eq!(block_bounds(0), (ARC_TASK_ID_POOL_START, 2_000_999_999));
        let last = block_bounds(ARC_TASK_ID_BLOCK_COUNT - 1);
        assert_eq!(last.1, ARC_TASK_ID_POOL_END);
        assert!(last.0 <= last.1);
        assert!(last.1 < i32::MAX as u32);
    }

    #[test]
    fn allocator_returns_sequential_ids_and_reports_exhaustion() {
        let allocator = ArcTaskIdAllocator::for_test(2_000_000_000, 2_000_000_002);
        assert_eq!(allocator.allocate().unwrap(), 2_000_000_000);
        assert_eq!(allocator.allocate().unwrap(), 2_000_000_001);
        assert_eq!(allocator.allocate().unwrap(), 2_000_000_002);
        assert_eq!(
            allocator.allocate().unwrap_err().kind(),
            io::ErrorKind::AddrNotAvailable
        );
    }

    #[test]
    fn flock_assigns_distinct_blocks_and_releases_on_drop() {
        let directory = temporary_directory("contention");
        let first = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect("first process reserves the first block");
        let first_range = first.block_range();
        let second = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect("second process advances to a free block");
        assert_ne!(first_range, second.block_range());

        let first_path = directory.join(format!("{}-{}.lock", first_range.0, first_range.1));
        assert_eq!(
            fs::metadata(&first_path)
                .expect("lock file remains inspectable")
                .len(),
            0
        );

        drop(first);
        drop(second);
        let reclaimed = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect("released flock can be reused");
        assert_eq!(reclaimed.block_range(), first_range);
        drop(reclaimed);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn block_probe_wraps_random_start_index_without_overflow() {
        let directory = temporary_directory("random-index-wrap");
        let allocator = ArcTaskIdAllocator::acquire_in_directory(&directory, u32::MAX)
            .expect("a high random start index must wrap through the block table");
        assert_eq!(
            allocator.block_range(),
            block_bounds(u32::MAX % ARC_TASK_ID_BLOCK_COUNT)
        );
        drop(allocator);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }
}
