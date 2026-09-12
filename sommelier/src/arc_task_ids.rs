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
//! numeric block with an advisory file lock and allocate monotonically from
//! that block.  The lock is held by the open descriptor for the lifetime of
//! the allocator, so a second process skips the block while the first is live.

use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
    next_id: u64,
    end_id: u32,
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
        validate_directory(&runtime_dir, None)?;

        let block_dir = runtime_dir.join("sommelier").join("arc-task-blocks");
        ensure_private_directory(&block_dir)?;

        let mut random = [0_u8; 4];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let start_index = u32::from_ne_bytes(random) % ARC_ID_BLOCK_COUNT;
        Self::acquire_in_directory_with_guard(&block_dir, &runtime_dir, start_index)
    }

    #[cfg(test)]
    fn acquire_in_directory(directory: &Path, start_index: u32) -> io::Result<Arc<Self>> {
        Self::acquire_in_directory_with_guard(directory, directory, start_index)
    }

    fn acquire_in_directory_with_guard(
        directory: &Path,
        guard_directory: &Path,
        start_index: u32,
    ) -> io::Result<Arc<Self>> {
        validate_directory(directory, Some(0o700))?;
        validate_directory(guard_directory, None)?;
        // Keep a descriptor for the validated directory and use openat for
        // every lock file. Re-opening a lock pathname after validation could
        // otherwise redirect an acquisition into a replacement directory.
        let directory_file = open_directory(directory)?;
        let directory_metadata = directory_file.metadata()?;
        let path_metadata = fs::symlink_metadata(directory)?;
        if path_metadata.dev() != directory_metadata.dev()
            || path_metadata.ino() != directory_metadata.ino()
        {
            return Err(io::Error::other(
                "ARC identity lock directory changed while opening",
            ));
        }
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
                    let current_directory_metadata = directory_file.metadata()?;
                    if current_directory_metadata.dev() != directory_metadata.dev()
                        || current_directory_metadata.ino() != directory_metadata.ino()
                    {
                        return Err(io::Error::other(
                            "ARC identity lock directory changed while acquiring",
                        ));
                    }
                    info!(
                        "Reserved ARC identity block {}-{} using {}",
                        start_id,
                        end_id,
                        path.display()
                    );
                    return Ok(Arc::new(Self {
                        block: Mutex::new(BlockState {
                            lock_file: Some(file),
                            directory_guard: Some(directory_guard),
                            start_id,
                            next_id: u64::from(start_id),
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
        if block.next_id > u64::from(block.end_id) {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "ARC identity block is exhausted",
            ));
        }
        let id = block.next_id as u32;
        debug_assert!(id >= block.start_id);
        block.next_id += 1;
        Ok(id)
    }

    #[cfg(test)]
    pub(crate) fn for_test(start_id: u32, end_id: u32) -> Arc<Self> {
        assert!(start_id <= end_id);
        Arc::new(Self {
            block: Mutex::new(BlockState {
                lock_file: None,
                directory_guard: None,
                start_id,
                next_id: u64::from(start_id),
                end_id,
            }),
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
    let guard_directory_file = open_directory(guard_directory)?;
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
            match read_directory_identity(&mut guard, &guard_path)? {
                None => write_directory_identity(&mut guard, &guard_path, expected)?,
                Some(actual) if actual != expected => {
                    write_directory_identity(&mut guard, &guard_path, expected)?
                }
                Some(_) => {}
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
    fs::create_dir_all(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o700))?;
    validate_directory(path, Some(0o700))
}

fn validate_directory(path: &Path, expected_mode: Option<u32>) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("ARC identity path must be absolute: {}", path.display()),
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sommelier-arc-id-block-{}-{nonce}",
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
        let first = ArcIdAllocator::acquire_in_directory(&directory, 0).expect("first block");
        let first_range = first.block_range();
        let second = ArcIdAllocator::acquire_in_directory(&directory, 0).expect("second block");
        assert_ne!(first_range, second.block_range());
        drop(first);
        drop(second);
        let reclaimed = ArcIdAllocator::acquire_in_directory(&directory, 0)
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

        let first = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
            .expect("first allocator reserves the block");
        fs::remove_dir_all(&directory).expect("remove block directory");
        fs::create_dir(&directory).expect("recreate block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict recreated block directory");

        let error = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
            .expect_err("a recreated directory must not split the lock namespace");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        drop(first);
        let recovered = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
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

        let first = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0)
            .expect("first allocator reserves the block");
        fs::remove_dir_all(&sommelier).expect("remove Sommelier directory");
        fs::create_dir(&sommelier).expect("recreate Sommelier directory");
        fs::set_permissions(&sommelier, Permissions::from_mode(0o700))
            .expect("restrict recreated Sommelier directory");
        fs::create_dir(&directory).expect("recreate block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict recreated block directory");

        let error = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0)
            .expect_err("a recreated parent must not split the lock namespace");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        drop(first);
        let recovered = ArcIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0)
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
}
