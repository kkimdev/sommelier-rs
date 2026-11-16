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

//! Process-shared allocation for fabricated ARC policy identities.
//!
//! The ARC bounds workaround needs one identity per live surface.  A local
//! process counter is not sufficient because multiple Sommelier processes can
//! be alive at once (and PID/serial truncation can collide).  Reserve one
//! numeric block with an advisory file lock and allocate from that block,
//! starting at a random offset and wrapping within it.  The lock is held by
//! the open descriptor for the lifetime of the allocator, so a second process
//! skips the block while the first is live.
//!
//! Limits of the guarantee:
//!
//! - Exclusivity holds only between proxies that share one
//!   `$XDG_RUNTIME_DIR`.  Proxies in other runtime directories (other
//!   containers or VMs attached to the same host compositor) coordinate
//!   nothing; the random block and random in-block offset only make an
//!   overlap unlikely.
//! - The generation guard lives in `$XDG_RUNTIME_DIR` itself.  Replacing the
//!   whole runtime directory (or deleting the guard) while an allocator is
//!   alive starts an independent namespace, with the same caveat as above.
//! - Each process reserves one block and never refills it; identities are not
//!   recycled.  After the block is exhausted, allocation fails and the ARC
//!   policy is skipped for new surfaces instead of reusing an identity.

use std::ffi::CString;
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use log::{info, warn};

/// Keep fabricated values positive and below the signed 32-bit sentinel used
/// by the host ARC parser.  This pool is intentionally far from normal ARC
/// task allocation; the namespace remains experimental and host-dependent.
pub(crate) const ARC_ID_POOL_START: u32 = 2_000_000_000;
pub(crate) const ARC_ID_POOL_END: u32 = i32::MAX as u32 - 1;
pub(crate) const ARC_ID_BLOCK_SIZE: u32 = 1_000_000;
const ARC_ID_BLOCK_COUNT: u32 = (ARC_ID_POOL_END - ARC_ID_POOL_START) / ARC_ID_BLOCK_SIZE + 1;
const DIRECTORY_GUARD_FILE: &str = ".arc-task-blocks.guard";

#[derive(Debug)]
struct BlockState {
    /// The descriptor owns the flock for the block.  `None` is used only by
    /// deterministic unit-test allocators.
    lock_file: Option<File>,
    /// A shared lock on a stable parent guard prevents a deleted and
    /// recreated lock directory from becoming a second namespace while an
    /// older allocator still owns a block.
    directory_guard: Option<File>,
    start_id: u32,
    end_id: u32,
    /// Offset from `start_id` of the next identity; wraps within the block.
    next_offset: u64,
    /// Identities handed out so far; the block is exhausted once this equals
    /// the block length, regardless of where the offset started.
    allocated: u64,
}

impl BlockState {
    fn new(
        lock_file: Option<File>,
        directory_guard: Option<File>,
        start_id: u32,
        end_id: u32,
        offset_seed: u32,
    ) -> Self {
        debug_assert!(start_id <= end_id);
        let mut block = Self {
            lock_file,
            directory_guard,
            start_id,
            end_id,
            next_offset: 0,
            allocated: 0,
        };
        block.next_offset = u64::from(offset_seed) % block.len();
        block
    }

    fn len(&self) -> u64 {
        u64::from(self.end_id) - u64::from(self.start_id) + 1
    }
}

impl Drop for BlockState {
    fn drop(&mut self) {
        // The descriptor is the lifetime of the process-shared flock.  Take it
        // explicitly so the ownership invariant is visible to the compiler
        // (and to reviewers) instead of leaving `lock_file` as an unread
        // bookkeeping field.
        let _ = self.lock_file.take();
        let _ = self.directory_guard.take();
    }
}

/// One process-wide allocator shared by all client connections.
#[derive(Debug)]
pub(crate) struct ArcIdAllocator {
    block: Mutex<BlockState>,
}

static PROCESS_ALLOCATOR: OnceLock<Option<Arc<ArcIdAllocator>>> = OnceLock::new();

/// Return the allocator reserved for this Sommelier process.
///
/// A failure disables the ARC placement path for the process instead of
/// falling back to an identity that can collide with another proxy.
pub(crate) fn process_allocator() -> Option<Arc<ArcIdAllocator>> {
    PROCESS_ALLOCATOR
        .get_or_init(|| match ArcIdAllocator::acquire() {
            Ok(allocator) => Some(allocator),
            Err(error) => {
                warn!("Unable to reserve an ARC identity block: {}", error);
                None
            }
        })
        .clone()
}

impl ArcIdAllocator {
    fn acquire() -> io::Result<Arc<Self>> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "XDG_RUNTIME_DIR is required for ARC identity allocation",
            )
        })?;
        let runtime_dir = PathBuf::from(runtime_dir);
        open_validated_directory(&runtime_dir, None)?;

        let block_dir = runtime_dir.join("sommelier").join("arc-task-blocks");
        ensure_private_directory(&block_dir)?;

        let mut random = File::open("/dev/urandom")?;
        let mut index_bytes = [0_u8; 4];
        let mut offset_bytes = [0_u8; 4];
        random.read_exact(&mut index_bytes)?;
        random.read_exact(&mut offset_bytes)?;
        let start_index = u32::from_ne_bytes(index_bytes) % ARC_ID_BLOCK_COUNT;
        let offset_seed = u32::from_ne_bytes(offset_bytes);
        Self::acquire_in_directory_with_guard(&block_dir, &runtime_dir, start_index, offset_seed)
    }

    #[cfg(test)]
    fn acquire_in_directory(
        directory: &Path,
        start_index: u32,
        offset_seed: u32,
    ) -> io::Result<Arc<Self>> {
        Self::acquire_in_directory_with_guard(directory, directory, start_index, offset_seed)
    }

    fn acquire_in_directory_with_guard(
        directory: &Path,
        guard_directory: &Path,
        start_index: u32,
        offset_seed: u32,
    ) -> io::Result<Arc<Self>> {
        // Keep a descriptor for the validated directory and use openat for
        // every lock file. Re-opening a lock pathname after validation could
        // otherwise redirect an acquisition into a replacement directory.
        let directory_file = open_validated_directory(directory, Some(0o700))?;
        let directory_metadata = directory_file.metadata()?;
        let path_metadata = fs::symlink_metadata(directory)?;
        if path_metadata.dev() != directory_metadata.dev()
            || path_metadata.ino() != directory_metadata.ino()
        {
            return Err(io::Error::other(
                "ARC identity lock directory changed while opening",
            ));
        }
        // The guard records this directory generation while any allocator is
        // alive, so a later replacement of the directory is rejected by every
        // subsequent acquisition. That shared guard, not a post-lock path
        // re-check (which would itself race), is what keeps one namespace.
        let directory_guard = establish_directory_guard(&directory_metadata, guard_directory)?;
        let start_index = start_index % ARC_ID_BLOCK_COUNT;
        for offset in 0..ARC_ID_BLOCK_COUNT {
            let index = (start_index + offset) % ARC_ID_BLOCK_COUNT;
            let (start_id, end_id) = block_bounds(index);
            let name = format!("{start_id}-{end_id}.lock");
            let path = directory.join(&name);
            let file = open_lock_file_at(directory_file.as_raw_fd(), &path, &name)?;

            match try_lock(&file) {
                Ok(()) => {
                    info!(
                        "Reserved ARC identity block {}-{} using {}",
                        start_id,
                        end_id,
                        path.display()
                    );
                    return Ok(Arc::new(Self {
                        block: Mutex::new(BlockState::new(
                            Some(file),
                            Some(directory_guard),
                            start_id,
                            end_id,
                            offset_seed,
                        )),
                    }));
                }
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
                    ) =>
                {
                    // Another live proxy owns this block; try the next one.
                }
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "all ARC identity blocks are already reserved",
        ))
    }

    pub(crate) fn allocate(&self) -> io::Result<u32> {
        let mut block = self
            .block
            .lock()
            .map_err(|_| io::Error::other("ARC identity allocator mutex poisoned"))?;
        let len = block.len();
        if block.allocated >= len {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "ARC identity block is exhausted",
            ));
        }
        let id = u32::try_from(u64::from(block.start_id) + block.next_offset)
            .map_err(|_| io::Error::other("ARC identity offset out of range"))?;
        debug_assert!(id >= block.start_id && id <= block.end_id);
        block.next_offset = (block.next_offset + 1) % len;
        block.allocated += 1;
        Ok(id)
    }

    #[cfg(test)]
    pub(crate) fn for_test(start_id: u32, end_id: u32) -> Arc<Self> {
        Self::for_test_with_offset(start_id, end_id, 0)
    }

    #[cfg(test)]
    fn for_test_with_offset(start_id: u32, end_id: u32, offset_seed: u32) -> Arc<Self> {
        assert!(start_id <= end_id);
        Arc::new(Self {
            block: Mutex::new(BlockState::new(None, None, start_id, end_id, offset_seed)),
        })
    }

    #[cfg(test)]
    fn block_range(&self) -> (u32, u32) {
        let block = self.block.lock().expect("allocator mutex");
        (block.start_id, block.end_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

fn directory_identity(metadata: &Metadata) -> DirectoryIdentity {
    DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
    options.open(path)
}

/// Open a directory without following a final symlink and validate the
/// owner and mode of the opened descriptor, so a path swapped between a
/// check and the open cannot substitute an unvalidated directory.
fn open_validated_directory(path: &Path, expected_mode: Option<u32>) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("ARC identity path must be absolute: {}", path.display()),
        ));
    }
    let file = open_directory(path)?;
    validate_directory_metadata(path, &file.metadata()?, expected_mode)?;
    Ok(file)
}

fn read_directory_identity(file: &mut File, path: &Path) -> io::Result<Option<DirectoryIdentity>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let fields = std::str::from_utf8(&bytes)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid ARC guard: {}", path.display()),
            )
        })?
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid ARC directory identity guard: {}", path.display()),
        ));
    }
    let parse = |field: &str| {
        field.parse::<u64>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid ARC directory identity guard: {}", path.display()),
            )
        })
    };
    Ok(Some(DirectoryIdentity {
        device: parse(fields[0])?,
        inode: parse(fields[1])?,
    }))
}

fn write_directory_identity(
    file: &mut File,
    path: &Path,
    identity: DirectoryIdentity,
) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{} {}", identity.device, identity.inode).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "unable to write ARC directory identity guard {}: {error}",
                path.display()
            ),
        )
    })?;
    file.sync_data()
}

fn try_flock_exclusive(file: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn flock_shared(file: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn establish_directory_guard(
    directory_metadata: &Metadata,
    guard_directory: &Path,
) -> io::Result<File> {
    let guard_directory_file = open_validated_directory(guard_directory, None)?;
    let guard_path = guard_directory.join(DIRECTORY_GUARD_FILE);
    let mut guard = open_lock_file_at(
        guard_directory_file.as_raw_fd(),
        &guard_path,
        DIRECTORY_GUARD_FILE,
    )?;
    let expected = directory_identity(directory_metadata);

    match try_flock_exclusive(&guard) {
        Ok(()) => {
            // No live allocator holds the shared guard, so a new directory
            // generation may adopt the marker. Downgrade to a shared lock
            // before returning so this allocator participates in the guard.
            // An unparsable marker (for example a torn write) has no live
            // owner either, so it is replaced like a stale generation.
            let current = match read_directory_identity(&mut guard, &guard_path) {
                Ok(identity) => identity,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    warn!("Replacing unreadable ARC directory identity guard: {error}");
                    None
                }
                Err(error) => return Err(error),
            };
            if current != Some(expected) {
                write_directory_identity(&mut guard, &guard_path, expected)?;
            }
            flock_shared(&guard)?;
        }
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            ) =>
        {
            // Another allocator owns the current generation. A mismatch
            // proves the lock directory was replaced while it was alive.
            flock_shared(&guard)?;
            let actual = read_directory_identity(&mut guard, &guard_path)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ARC directory identity guard is empty",
                )
            })?;
            if actual != expected {
                return Err(io::Error::other(
                    "ARC identity lock directory was replaced while in use",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(guard)
}

fn open_lock_file_at(directory_fd: RawFd, path: &Path, file_name: &str) -> io::Result<File> {
    let c_file_name = CString::new(file_name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ARC lock name contains NUL: {}", path.display()),
        )
    })?;
    let flags = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let fd = unsafe {
        libc::openat(
            directory_fd,
            c_file_name.as_ptr(),
            flags | libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
    };
    if fd >= 0 {
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_permissions(Permissions::from_mode(0o600))?;
        validate_lock_file(path, &file)?;
        return Ok(file);
    }
    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::AlreadyExists {
        return Err(error);
    }
    let fd = unsafe { libc::openat(directory_fd, c_file_name.as_ptr(), flags | libc::O_RDWR) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    validate_lock_file(path, &file)?;
    Ok(file)
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
    debug_assert!(index < ARC_ID_BLOCK_COUNT);
    let start_id = ARC_ID_POOL_START + index * ARC_ID_BLOCK_SIZE;
    let end_id = start_id
        .saturating_add(ARC_ID_BLOCK_SIZE - 1)
        .min(ARC_ID_POOL_END);
    (start_id, end_id)
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    // New components are created private. An existing leaf is tightened
    // through its descriptor: the O_NOFOLLOW open rejects a symlink, so the
    // chmod can never be redirected to the link target.
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let directory = open_directory(path)?;
    directory.set_permissions(Permissions::from_mode(0o700))?;
    validate_directory_metadata(path, &directory.metadata()?, Some(0o700))
}

fn validate_directory_metadata(
    path: &Path,
    metadata: &Metadata,
    expected_mode: Option<u32>,
) -> io::Result<()> {
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("ARC identity path is not a directory: {}", path.display()),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "ARC identity directory is not user-owned: {}",
                path.display()
            ),
        ));
    }
    let mode = metadata.mode() & 0o7777;
    if let Some(expected_mode) = expected_mode {
        if mode != expected_mode {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("ARC identity directory has unsafe mode: {}", path.display()),
            ));
        }
    } else if mode & 0o077 != 0 || mode & 0o700 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "ARC runtime directory is not owner-only: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_lock_file(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("ARC identity lock file is unsafe: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        static SEQUENCE: AtomicU32 = AtomicU32::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sommelier-arc-id-block-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test directory");
        fs::set_permissions(&path, Permissions::from_mode(0o700)).expect("set test mode");
        path
    }

    #[test]
    fn allocator_is_sequential_and_reports_exhaustion() {
        let allocator = ArcIdAllocator::for_test(2_000_000_000, 2_000_000_002);
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
        let directory = temporary_directory();
        let first = ArcIdAllocator::acquire_in_directory(&directory, 0, 0).expect("first block");
        let first_range = first.block_range();
        let second = ArcIdAllocator::acquire_in_directory(&directory, 0, 0).expect("second block");
        assert_ne!(first_range, second.block_range());
        drop(first);
        drop(second);
        let reclaimed = ArcIdAllocator::acquire_in_directory(&directory, 0, 0)
            .expect("released block can be reused");
        assert_eq!(reclaimed.block_range(), first_range);
        drop(reclaimed);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn recreated_block_directory_is_rejected_while_old_allocator_is_alive() {
        let parent = temporary_directory();
        let directory = parent.join("arc-task-blocks");
        fs::create_dir(&directory).expect("create block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict block directory");

        let first = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0, 0)
            .expect("first allocator reserves the block");
        fs::remove_dir_all(&directory).expect("remove block directory");
        fs::create_dir(&directory).expect("recreate block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict recreated block directory");

        let error = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0, 0)
            .expect_err("a recreated directory must not split the lock namespace");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        drop(first);
        let recovered = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0, 0)
            .expect("the marker can be adopted after the old allocator exits");
        drop(recovered);
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn recreated_sommelier_parent_is_rejected_while_old_allocator_is_alive() {
        let runtime = temporary_directory();
        let sommelier = runtime.join("sommelier");
        let directory = sommelier.join("arc-task-blocks");
        fs::create_dir(&sommelier).expect("create Sommelier directory");
        fs::set_permissions(&sommelier, Permissions::from_mode(0o700))
            .expect("restrict Sommelier directory");
        fs::create_dir(&directory).expect("create block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict block directory");

        let first = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0, 0)
            .expect("first allocator reserves the block");
        fs::remove_dir_all(&sommelier).expect("remove Sommelier directory");
        fs::create_dir(&sommelier).expect("recreate Sommelier directory");
        fs::set_permissions(&sommelier, Permissions::from_mode(0o700))
            .expect("restrict recreated Sommelier directory");
        fs::create_dir(&directory).expect("recreate block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict recreated block directory");

        let error = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0, 0)
            .expect_err("a recreated parent must not split the lock namespace");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        drop(first);
        let recovered = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0, 0)
            .expect("the marker can be adopted after the old allocator exits");
        drop(recovered);
        fs::remove_dir_all(runtime).expect("remove test directories");
    }

    #[test]
    fn pool_excludes_signed_int_max() {
        assert!(ARC_ID_POOL_END < i32::MAX as u32);
        assert_eq!(block_bounds(0).0, ARC_ID_POOL_START);
        assert_eq!(block_bounds(ARC_ID_BLOCK_COUNT - 1).1, ARC_ID_POOL_END);
    }

    fn first_block_lock_name() -> String {
        let (start_id, end_id) = block_bounds(0);
        format!("{start_id}-{end_id}.lock")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("stat").mode() & 0o7777
    }

    #[test]
    fn allocator_starts_at_offset_and_wraps_before_reporting_exhaustion() {
        // A seed of 4 in a three-identity block starts at offset 1.
        let allocator = ArcIdAllocator::for_test_with_offset(10, 12, 4);
        assert_eq!(allocator.allocate().unwrap(), 11);
        assert_eq!(allocator.allocate().unwrap(), 12);
        assert_eq!(allocator.allocate().unwrap(), 10);
        for _ in 0..2 {
            assert_eq!(
                allocator.allocate().unwrap_err().kind(),
                io::ErrorKind::AddrNotAvailable
            );
        }
    }

    #[test]
    fn offset_seed_stays_inside_the_partial_last_block() {
        let directory = temporary_directory();
        let allocator =
            ArcIdAllocator::acquire_in_directory(&directory, ARC_ID_BLOCK_COUNT - 1, u32::MAX)
                .expect("last block");
        let (start_id, end_id) = allocator.block_range();
        assert_eq!(end_id, ARC_ID_POOL_END);
        let id = allocator.allocate().expect("identity");
        assert!((start_id..=end_id).contains(&id));
        drop(allocator);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn independent_runtime_directories_sharing_a_block_start_at_distinct_offsets() {
        // Proxies in different runtime directories cannot see each other's
        // locks. Choosing the same block must not imply the same identities.
        let first_directory = temporary_directory();
        let second_directory = temporary_directory();
        let first =
            ArcIdAllocator::acquire_in_directory(&first_directory, 3, 17).expect("first namespace");
        let second = ArcIdAllocator::acquire_in_directory(&second_directory, 3, 900_017)
            .expect("second namespace");
        let (start_id, _) = first.block_range();
        assert_eq!(first.block_range(), second.block_range());
        assert_eq!(first.allocate().unwrap(), start_id + 17);
        assert_eq!(second.allocate().unwrap(), start_id + 900_017);
        drop((first, second));
        fs::remove_dir_all(first_directory).expect("remove first directory");
        fs::remove_dir_all(second_directory).expect("remove second directory");
    }

    #[test]
    fn unparsable_guard_marker_is_replaced_when_no_allocator_is_live() {
        let parent = temporary_directory();
        let directory = parent.join("arc-task-blocks");
        fs::create_dir(&directory).expect("create block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict block directory");
        let guard_path = parent.join(DIRECTORY_GUARD_FILE);
        fs::write(&guard_path, b"12345").expect("write torn marker");
        fs::set_permissions(&guard_path, Permissions::from_mode(0o600)).expect("restrict guard");

        let allocator = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0, 0)
            .expect("a torn marker without a live owner is recoverable");
        let metadata = fs::metadata(&directory).expect("stat block directory");
        assert_eq!(
            fs::read_to_string(&guard_path).expect("read guard"),
            format!("{} {}\n", metadata.dev(), metadata.ino())
        );
        drop(allocator);
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn symlinked_lock_file_is_rejected_without_touching_its_target() {
        let directory = temporary_directory();
        let elsewhere = temporary_directory();
        let target = elsewhere.join("target");
        fs::write(&target, b"keep").expect("write target");
        fs::set_permissions(&target, Permissions::from_mode(0o644)).expect("target mode");
        std::os::unix::fs::symlink(&target, directory.join(first_block_lock_name()))
            .expect("plant lock symlink");

        assert!(ArcIdAllocator::acquire_in_directory(&directory, 0, 0).is_err());
        assert_eq!(fs::read(&target).expect("read target"), b"keep");
        assert_eq!(mode_of(&target), 0o644);
        fs::remove_dir_all(directory).expect("remove test directory");
        fs::remove_dir_all(elsewhere).expect("remove target directory");
    }

    #[test]
    fn lock_file_with_unsafe_mode_is_rejected() {
        let directory = temporary_directory();
        let lock_path = directory.join(first_block_lock_name());
        fs::write(&lock_path, b"").expect("create lock file");
        fs::set_permissions(&lock_path, Permissions::from_mode(0o644)).expect("lock mode");

        let error = ArcIdAllocator::acquire_in_directory(&directory, 0, 0)
            .expect_err("a group-readable lock file is not trusted");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn block_directory_with_unsafe_mode_is_rejected() {
        let directory = temporary_directory();
        fs::set_permissions(&directory, Permissions::from_mode(0o755)).expect("widen mode");

        let error = ArcIdAllocator::acquire_in_directory(&directory, 0, 0)
            .expect_err("a non-private block directory is not trusted");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn private_directory_setup_does_not_chmod_through_a_symlink() {
        let runtime = temporary_directory();
        let target = runtime.join("target");
        fs::create_dir(&target).expect("create target");
        fs::set_permissions(&target, Permissions::from_mode(0o755)).expect("target mode");
        let link = runtime.join("arc-task-blocks");
        std::os::unix::fs::symlink(&target, &link).expect("plant directory symlink");

        assert!(ensure_private_directory(&link).is_err());
        assert_eq!(mode_of(&target), 0o755);
        fs::remove_dir_all(runtime).expect("remove test directories");
    }

    #[test]
    fn private_directory_setup_creates_and_tightens_owner_only_directories() {
        let runtime = temporary_directory();
        let created = runtime.join("sommelier").join("arc-task-blocks");
        ensure_private_directory(&created).expect("create private directory");
        assert_eq!(mode_of(&created), 0o700);

        fs::set_permissions(&created, Permissions::from_mode(0o755)).expect("widen mode");
        ensure_private_directory(&created).expect("tighten existing directory");
        assert_eq!(mode_of(&created), 0o700);
        fs::remove_dir_all(runtime).expect("remove test directories");
    }
}
