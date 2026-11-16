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

//! Fabricated ARC task IDs for the `SOMMELIER_WINDOW_BOUNDS_AS_ARC` policy.
//!
//! The pool, the per-process block reservation, its exclusivity scope and its
//! failure behavior are specified once, in the "Sommelier's task-ID
//! allocator" section of `sommelier/docs/ARC_TASK_AND_SESSION_IDS.md`.
//!
//! [`ArcIdAllocator`] is the pure in-memory cursor over one block;
//! [`process_allocator`] reserves the process's block with a lock file and
//! keeps that lock until the process exits.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use log::{debug, info, warn};

/// First ID of the private pool, far above the task IDs of realistic Android
/// users.
pub(crate) const ARC_ID_POOL_START: u32 = 2_000_000_000;
/// Last ID of the pool. `i32::MAX` is excluded because the host parses the
/// suffix as a signed integer and treats that value as a sentinel.
pub(crate) const ARC_ID_POOL_END: u32 = i32::MAX as u32 - 1;
/// IDs per reserved block. The last block of the pool is partial.
pub(crate) const ARC_ID_BLOCK_SIZE: u32 = 1_000_000;
const ARC_ID_BLOCK_COUNT: u32 = (ARC_ID_POOL_END - ARC_ID_POOL_START) / ARC_ID_BLOCK_SIZE + 1;

/// Lock directory, relative to `$XDG_RUNTIME_DIR`.
const LOCK_DIRECTORY: &str = "sommelier/arc-task-blocks";

/// One block of ARC task IDs. IDs are handed out in order from a random
/// offset, wrapping inside the block, and each at most once.
#[derive(Debug)]
pub(crate) struct ArcIdAllocator {
    first_id: u32,
    len: u32,
    start_offset: u32,
    /// IDs handed out so far; never exceeds `len`.
    issued: Mutex<u32>,
}

impl ArcIdAllocator {
    /// Cover `first_id..=last_id`, starting at `offset_seed % len`.
    fn new(first_id: u32, last_id: u32, offset_seed: u32) -> Self {
        assert!(first_id <= last_id && last_id - first_id < ARC_ID_BLOCK_SIZE);
        let len = last_id - first_id + 1;
        Self {
            first_id,
            len,
            start_offset: offset_seed % len,
            issued: Mutex::new(0),
        }
    }

    fn last_id(&self) -> u32 {
        self.first_id + (self.len - 1)
    }

    /// Return the next unused ID, or `None` once the block is exhausted.
    ///
    /// Exhaustion is logged once, when the last ID is handed out.
    pub(crate) fn allocate(&self) -> Option<u32> {
        let issued = {
            // A poisoned lock still holds a valid count: the critical section
            // below cannot panic between reading and writing it.
            let mut issued = self
                .issued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *issued == self.len {
                return None;
            }
            *issued += 1;
            *issued - 1
        };
        if issued + 1 == self.len {
            warn!(
                "ARC task-ID block {}..={} is exhausted; later surfaces get no ARC identity",
                self.first_id,
                self.last_id()
            );
        }
        Some(self.first_id + (self.start_offset + issued) % self.len)
    }

    /// An unreserved allocator for tests, which never share a namespace.
    #[cfg(test)]
    pub(crate) fn for_test(first_id: u32, last_id: u32) -> &'static Self {
        Box::leak(Box::new(Self::new(first_id, last_id, 0)))
    }

    /// A full-size test block, like the one a process reserves.
    #[cfg(test)]
    pub(crate) fn block_for_test() -> &'static Self {
        Self::for_test(
            ARC_ID_POOL_START,
            ARC_ID_POOL_START + (ARC_ID_BLOCK_SIZE - 1),
        )
    }

    /// A test allocator with no ID left, as after exhaustion.
    #[cfg(test)]
    pub(crate) fn exhausted_for_test() -> &'static Self {
        let allocator = Self::for_test(ARC_ID_POOL_START, ARC_ID_POOL_START);
        assert!(allocator.allocate().is_some());
        allocator
    }
}

/// Return the inclusive ID range of block `index`.
fn block_bounds(index: u32) -> (u32, u32) {
    assert!(index < ARC_ID_BLOCK_COUNT);
    let first_id = ARC_ID_POOL_START + index * ARC_ID_BLOCK_SIZE;
    let last_id = (first_id + (ARC_ID_BLOCK_SIZE - 1)).min(ARC_ID_POOL_END);
    (first_id, last_id)
}

/// The process's block: its IDs and the lock that keeps other proxies off
/// them.
#[derive(Debug)]
struct ReservedBlock {
    ids: ArcIdAllocator,
    /// The open descriptor holds the `flock`; it is never read.
    _lock: File,
}

/// Never dropped, so a reserved block stays locked until the process exits.
static PROCESS_BLOCK: OnceLock<Option<ReservedBlock>> = OnceLock::new();

/// Return this process's allocator, reserving its block on the first call.
///
/// `PlacementConfig::from_environment` calls this once, at startup, when
/// `SOMMELIER_WINDOW_BOUNDS_AS_ARC` is set. A failed reservation is logged
/// once and cached for the process: its causes (no private runtime
/// directory, no lock directory, no randomness, every block taken) do not
/// clear while the process runs.
pub(crate) fn process_allocator() -> Option<&'static ArcIdAllocator> {
    PROCESS_BLOCK
        .get_or_init(|| match reserve_process_block() {
            Ok(block) => Some(block),
            Err(error) => {
                warn!(
                    "ARC bounds placement is disabled for this process: \
                     cannot reserve an ARC task-ID block: {error}"
                );
                None
            }
        })
        .as_ref()
        .map(|block| &block.ids)
}

fn reserve_process_block() -> io::Result<ReservedBlock> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    reserve_block(&runtime_dir, random_u64()?)
}

/// Lock the first free block under `runtime_dir`.
///
/// The high half of `random` picks the first block to try and the low half
/// the start offset inside the reserved block. Any error on one block's lock
/// file skips that block; only a runtime or lock directory that cannot be
/// used is fatal.
fn reserve_block(runtime_dir: &Path, random: u64) -> io::Result<ReservedBlock> {
    require_private_directory(runtime_dir)?;
    let directory = runtime_dir.join(LOCK_DIRECTORY);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;

    let first_index = (random >> 32) as u32 % ARC_ID_BLOCK_COUNT;
    let offset_seed = random as u32;
    for step in 0..ARC_ID_BLOCK_COUNT {
        let (first_id, last_id) = block_bounds((first_index + step) % ARC_ID_BLOCK_COUNT);
        let path = directory.join(lock_file_name(first_id, last_id));
        match lock_exclusive(&path) {
            Ok(lock) => {
                info!("Reserved ARC task-ID block {first_id}..={last_id}");
                return Ok(ReservedBlock {
                    ids: ArcIdAllocator::new(first_id, last_id, offset_seed),
                    _lock: lock,
                });
            }
            // Another live proxy holds this block: the expected case.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                debug!("Skipping ARC task-ID block {}: {error}", path.display());
            }
            Err(error) => {
                warn!(
                    "Skipping unusable ARC task-ID block {}: {error}",
                    path.display()
                );
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::ResourceBusy,
        format!(
            "every ARC task-ID block in {} is locked or unusable",
            directory.display()
        ),
    ))
}

/// Older Sommelier builds use the same name, so mixed versions still exclude
/// each other.
fn lock_file_name(first_id: u32, last_id: u32) -> String {
    format!("{first_id}-{last_id}.lock")
}

/// Require `path` to be a directory that only this user can access, as the
/// XDG Base Directory spec requires of `$XDG_RUNTIME_DIR`. That keeps every
/// other user out of the lock directory, so the lock files need no checks of
/// their own.
fn require_private_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != user || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory private to this user", path.display()),
        ));
    }
    Ok(())
}

/// Open (creating if needed) one block's lock file and take an exclusive
/// `flock` without blocking.
fn lock_exclusive(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        // `create` requires write access; nothing is ever written.
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        // Never create or lock a file through a symlink, and never block
        // opening a FIFO or device; `flock` is unaffected by `O_NONBLOCK`.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    // `File::try_lock` is `flock(LOCK_EX | LOCK_NB)` on Linux.
    file.try_lock()?;
    Ok(file)
}

fn random_u64() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    loop {
        // SAFETY: `bytes` is valid for `bytes.len()` bytes of writes.
        let read = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        // Requests of up to 256 bytes are never short; only an interrupted
        // wait for the entropy pool at early boot fails transiently.
        if read == bytes.len() as isize {
            return Ok(u64::from_ne_bytes(bytes));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs::{self, Permissions};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    /// A temporary stand-in for `$XDG_RUNTIME_DIR`.
    fn runtime_dir(label: &str) -> TempDir {
        let directory = TempDir::new(label);
        fs::set_permissions(directory.path(), Permissions::from_mode(0o700))
            .expect("make the runtime directory private");
        directory
    }

    fn seeds(block: u32, offset: u32) -> u64 {
        u64::from(block) << 32 | u64::from(offset)
    }

    fn lock_path(runtime: &TempDir, block: u32) -> PathBuf {
        let (first_id, last_id) = block_bounds(block);
        runtime
            .path()
            .join(LOCK_DIRECTORY)
            .join(lock_file_name(first_id, last_id))
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("stat").mode() & 0o7777
    }

    #[test]
    fn pool_blocks_cover_the_pool_and_exclude_signed_int_max() {
        assert!(ARC_ID_POOL_END < i32::MAX as u32);
        assert_eq!(
            block_bounds(0),
            (ARC_ID_POOL_START, ARC_ID_POOL_START + ARC_ID_BLOCK_SIZE - 1)
        );
        assert_eq!(block_bounds(ARC_ID_BLOCK_COUNT - 1).1, ARC_ID_POOL_END);
        for index in 1..ARC_ID_BLOCK_COUNT {
            assert_eq!(block_bounds(index).0, block_bounds(index - 1).1 + 1);
        }
    }

    #[test]
    fn allocator_starts_at_the_offset_wraps_and_never_reuses_an_id() {
        // A seed of 4 in a three-ID block starts at offset 1.
        let allocator = ArcIdAllocator::new(10, 12, 4);
        assert_eq!(allocator.allocate(), Some(11));
        assert_eq!(allocator.allocate(), Some(12));
        assert_eq!(allocator.allocate(), Some(10));
        assert_eq!(allocator.allocate(), None);
        assert_eq!(allocator.allocate(), None);
    }

    #[test]
    fn offset_stays_inside_the_partial_last_block() {
        let (first_id, last_id) = block_bounds(ARC_ID_BLOCK_COUNT - 1);
        let allocator = ArcIdAllocator::new(first_id, last_id, u32::MAX);
        let id = allocator.allocate().expect("ID");
        assert!((first_id..=last_id).contains(&id));
    }

    #[test]
    fn reservation_uses_both_seeds_and_creates_a_private_lock_directory() {
        let runtime = runtime_dir("arc-seeds");
        let block = reserve_block(runtime.path(), seeds(3 + ARC_ID_BLOCK_COUNT, 17))
            .expect("reserve a block");
        assert_eq!(block.ids.allocate(), Some(block_bounds(3).0 + 17));
        assert_eq!(mode_of(&runtime.path().join(LOCK_DIRECTORY)), 0o700);
        assert_eq!(mode_of(&lock_path(&runtime, 3)), 0o600);
    }

    #[test]
    fn live_reservations_get_distinct_blocks_and_release_on_drop() {
        let runtime = runtime_dir("arc-exclusive");
        let first = reserve_block(runtime.path(), seeds(0, 0)).expect("first block");
        let second = reserve_block(runtime.path(), seeds(0, 0)).expect("second block");
        assert_eq!(first.ids.first_id, block_bounds(0).0);
        assert_eq!(second.ids.first_id, block_bounds(1).0);
        // A held block reports `WouldBlock`, which is skipped quietly.
        let held = lock_exclusive(&lock_path(&runtime, 0)).expect_err("block 0 is held");
        assert_eq!(held.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        let reclaimed = reserve_block(runtime.path(), seeds(0, 0)).expect("reclaimed block");
        assert_eq!(reclaimed.ids.first_id, block_bounds(0).0);
    }

    #[test]
    fn an_unusable_lock_file_skips_only_its_block() {
        let runtime = runtime_dir("arc-skip");
        let elsewhere = TempDir::new("arc-skip-target");
        let target = elsewhere.write("target", b"keep");
        fs::set_permissions(&target, Permissions::from_mode(0o644)).expect("target mode");
        fs::create_dir_all(runtime.path().join(LOCK_DIRECTORY)).expect("lock directory");
        std::os::unix::fs::symlink(&target, lock_path(&runtime, 0)).expect("plant symlink");
        fs::create_dir(lock_path(&runtime, 1)).expect("plant directory");
        let fifo = std::ffi::CString::new(lock_path(&runtime, 2).into_os_string().into_vec())
            .expect("path without NUL");
        // SAFETY: `fifo` is a valid NUL-terminated path.
        assert_eq!(
            unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) },
            0,
            "plant FIFO"
        );

        let block = reserve_block(runtime.path(), seeds(0, 0)).expect("a later block");
        assert_eq!(block.ids.first_id, block_bounds(3).0);
        assert_eq!(fs::read(&target).expect("read target"), b"keep");
        assert_eq!(mode_of(&target), 0o644);
    }

    #[test]
    fn an_existing_lock_file_is_used_whatever_its_mode() {
        // The private runtime directory is the trust boundary; a stray mode
        // on one lock file must not disable the policy.
        let runtime = runtime_dir("arc-mode");
        fs::create_dir_all(runtime.path().join(LOCK_DIRECTORY)).expect("lock directory");
        fs::write(lock_path(&runtime, 0), b"").expect("stray lock file");
        fs::set_permissions(lock_path(&runtime, 0), Permissions::from_mode(0o644))
            .expect("widen mode");

        let block = reserve_block(runtime.path(), seeds(0, 0)).expect("block 0");
        assert_eq!(block.ids.first_id, block_bounds(0).0);
    }

    #[test]
    fn reservation_fails_closed_when_no_block_can_be_locked() {
        let runtime = runtime_dir("arc-full");
        fs::create_dir_all(runtime.path().join(LOCK_DIRECTORY)).expect("lock directory");
        for block in 0..ARC_ID_BLOCK_COUNT {
            fs::create_dir(lock_path(&runtime, block)).expect("block every lock file");
        }
        let error = reserve_block(runtime.path(), seeds(0, 0)).expect_err("no free block");
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
    }

    #[test]
    fn a_runtime_directory_other_users_can_access_is_rejected() {
        let runtime = runtime_dir("arc-shared");
        fs::set_permissions(runtime.path(), Permissions::from_mode(0o755)).expect("widen mode");
        let error = reserve_block(runtime.path(), seeds(0, 0)).expect_err("shared runtime dir");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!runtime.path().join("sommelier").exists());
    }

    #[test]
    fn random_draws_differ() {
        // Two equal 64-bit draws have probability 2^-64.
        assert_ne!(random_u64().expect("random"), random_u64().expect("random"));
    }
}
