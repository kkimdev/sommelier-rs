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

use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
const DIRECTORY_GUARD_FILE: &str = ".arc-task-blocks.guard";

#[derive(Debug)]
struct ArcTaskIdBlock {
    /// The open descriptor keeps the block lock alive for the process.
    lock_file: Option<File>,
    /// A shared lock on a stable runtime-parent guard detects lock-directory
    /// deletion/recreation while another allocator still owns a block.
    directory_guard: Option<File>,
    start_id: u32,
    /// Keep the cursor one bit wider than a wire ID so the exhausted state
    /// remains representable even for the test-only `u32::MAX` endpoint.
    next_id: u64,
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
        let runtime_dir = PathBuf::from(runtime_dir);
        validate_runtime_directory(&runtime_dir)?;
        let sommelier_dir = runtime_dir.join("sommelier");
        ensure_private_directory(&sommelier_dir)?;
        let block_dir = sommelier_dir.join("arc-task-blocks");
        ensure_private_directory(&block_dir)?;

        let start_index = random_block_index()?;
        // Keep the generation guard in the validated XDG runtime directory,
        // not below `sommelier/`. A stale cleanup/restart can replace the
        // latter while an allocator still owns a block; a stable parent guard
        // makes that replacement fail closed instead of creating a second
        // lock namespace for the same numeric IDs.
        Self::acquire_in_directory_with_guard(&block_dir, &runtime_dir, start_index)
    }

    /// Reserve a block beginning at a deterministic index.
    ///
    /// This helper is shared with tests so they can exercise lock contention
    /// without changing the production random-selection path.
    #[cfg(test)]
    fn acquire_in_directory(directory: &Path, start_index: u32) -> io::Result<Arc<Self>> {
        Self::acquire_in_directory_with_guard(directory, directory, start_index)
    }

    /// Reserve a block while keeping the directory-generation guard in a
    /// stable parent directory. The production path uses the validated XDG
    /// runtime directory as the guard directory so deleting/recreating either
    /// `sommelier` or `arc-task-blocks` cannot silently create a second lock
    /// namespace.
    fn acquire_in_directory_with_guard(
        directory: &Path,
        guard_directory: &Path,
        start_index: u32,
    ) -> io::Result<Arc<Self>> {
        validate_private_directory(directory)?;
        // The production guard lives directly in XDG_RUNTIME_DIR. Preserve
        // that directory's owner-only policy (which may include harmless
        // sticky/setgid bits) instead of requiring the stricter 0700 mode
        // used for allocator-owned subdirectories.
        validate_runtime_directory(guard_directory)?;
        // Keep one descriptor for the validated directory and resolve every
        // lock pathname relative to it. Re-opening `directory/<range>.lock`
        // by path after validation would let a same-UID rename/recreate race
        // redirect this acquisition into a different directory inode.
        let directory_file = open_directory(directory)?;
        let directory_metadata = directory_file.metadata()?;
        let directory_guard = establish_directory_guard(&directory_metadata, guard_directory)?;
        let start_index = start_index % ARC_TASK_ID_BLOCK_COUNT;
        for offset in 0..ARC_TASK_ID_BLOCK_COUNT {
            let index = (start_index + offset) % ARC_TASK_ID_BLOCK_COUNT;
            let (start_id, end_id) = block_bounds(index);
            let file_name = format!("{start_id}-{end_id}.lock");
            let path = directory.join(&file_name);
            let file = open_lock_file_at(directory_file.as_raw_fd(), &path, &file_name)?;

            match try_lock(&file) {
                Ok(()) => {
                    // A directory replacement after the descriptor was
                    // opened is harmless for this process—the lock belongs
                    // to the already-validated inode. Keep the metadata read
                    // here as an invariant check for unusual filesystems that
                    // report an unstable directory descriptor.
                    let current_directory_metadata = directory_file.metadata()?;
                    if current_directory_metadata.dev() != directory_metadata.dev()
                        || current_directory_metadata.ino() != directory_metadata.ino()
                    {
                        return Err(security_error(
                            directory,
                            io::ErrorKind::Other,
                            "directory inode changed while acquiring a lock",
                        ));
                    }
                    info!(
                        "Reserved ARC task ID block {}-{} using {}",
                        start_id,
                        end_id,
                        path.display()
                    );
                    return Ok(Arc::new(Self {
                        block: Mutex::new(ArcTaskIdBlock {
                            lock_file: Some(file),
                            directory_guard: Some(directory_guard),
                            start_id,
                            next_id: start_id as u64,
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
        if block.next_id > u64::from(block.end_id) {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "ARC task ID block is exhausted",
            ));
        }

        let id = block.next_id as u32;
        debug_assert!(id >= block.start_id);
        block.next_id += 1;
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
                directory_guard: None,
                start_id,
                next_id: start_id as u64,
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
        let _ = self.directory_guard.take();
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

fn effective_uid() -> u32 {
    // `geteuid` has no failure mode and is available on every supported
    // Sommelier target (Linux/Unix).
    unsafe { libc::geteuid() }
}

fn security_error(path: &Path, kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(
        kind,
        format!("unsafe ARC task ID lock path {}: {message}", path.display()),
    )
}

fn validate_directory_identity(path: &Path, metadata: &Metadata) -> io::Result<()> {
    if !metadata.file_type().is_dir() {
        return Err(security_error(
            path,
            io::ErrorKind::NotADirectory,
            "expected a real directory",
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(security_error(
            path,
            io::ErrorKind::PermissionDenied,
            "directory is not owned by the effective user",
        ));
    }
    Ok(())
}

fn validate_directory_metadata(
    path: &Path,
    metadata: &Metadata,
    expected_mode: Option<u32>,
) -> io::Result<()> {
    validate_directory_identity(path, metadata)?;

    let mode = metadata.mode() & 0o7777;
    if let Some(expected_mode) = expected_mode {
        if mode != expected_mode {
            return Err(security_error(
                path,
                io::ErrorKind::PermissionDenied,
                "directory permissions are not exactly 0700",
            ));
        }
    } else if mode & 0o077 != 0 || mode & 0o700 != 0o700 {
        return Err(security_error(
            path,
            io::ErrorKind::PermissionDenied,
            "runtime directory must be owner-only",
        ));
    }
    Ok(())
}

fn open_directory(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options.open(path)
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

fn read_directory_identity(file: &mut File, path: &Path) -> io::Result<Option<DirectoryIdentity>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let fields = std::str::from_utf8(&bytes)
        .map_err(|_| security_error(path, io::ErrorKind::InvalidData, "guard is not UTF-8"))?
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() != 2 {
        return Err(security_error(
            path,
            io::ErrorKind::InvalidData,
            "guard has an invalid directory identity",
        ));
    }
    let parse_unsigned = |field: &str| {
        field.parse::<u64>().map_err(|_| {
            security_error(
                path,
                io::ErrorKind::InvalidData,
                "guard has an invalid directory identity",
            )
        })
    };
    let device = parse_unsigned(fields[0])?;
    let inode = parse_unsigned(fields[1])?;
    Ok(Some(DirectoryIdentity { device, inode }))
}

fn write_directory_identity(
    file: &mut File,
    path: &Path,
    identity: DirectoryIdentity,
) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{} {}", identity.device, identity.inode).map_err(|error| {
        security_error(
            path,
            error.kind(),
            "unable to write the directory identity guard",
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
            // An exclusive guard means no allocator still holds the old
            // directory generation. A changed directory can therefore be
            // adopted safely; while any allocator is alive, callers take the
            // shared-lock branch below and fail closed instead.
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
            // Wait for a concurrent initializer to finish writing the
            // marker, then retain a shared lock for this allocator's lifetime.
            flock_shared(&guard)?;
            let actual = read_directory_identity(&mut guard, &guard_path)?.ok_or_else(|| {
                security_error(
                    &guard_path,
                    io::ErrorKind::InvalidData,
                    "directory identity guard is empty",
                )
            })?;
            if actual != expected {
                return Err(security_error(
                    &guard_path,
                    io::ErrorKind::Other,
                    "directory was deleted and recreated while another allocator was alive",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(guard)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    validate_directory_metadata(path, &path_metadata, Some(0o700))?;
    let directory = open_directory(path)?;
    let descriptor_metadata = directory.metadata()?;
    validate_directory_metadata(path, &descriptor_metadata, Some(0o700))?;
    if path_metadata.dev() != descriptor_metadata.dev()
        || path_metadata.ino() != descriptor_metadata.ino()
    {
        return Err(security_error(
            path,
            io::ErrorKind::Other,
            "directory changed while it was being opened",
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let mut created = false;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_directory_identity(path, &metadata)?;
            // An older Sommelier may have created this parent with the
            // process umask (commonly 0755). It is safe to tighten
            // read/execute-only bits, but never repair a directory writable by
            // another class of users.
            if metadata.mode() & 0o022 != 0 {
                return Err(security_error(
                    path,
                    io::ErrorKind::PermissionDenied,
                    "directory is writable by a group or other user",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Request owner-only permissions at mkdir time. The process
            // umask can tighten this further, but cannot expose the new
            // directory to another class of users during normalization.
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => created = true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }

    let directory = open_directory(path)?;
    let path_metadata = fs::symlink_metadata(path)?;
    let descriptor_metadata = directory.metadata()?;
    if created {
        // The initial mkdir is subject to the process umask. Validate the
        // inode before normalizing its permissions through the descriptor.
        validate_directory_identity(path, &descriptor_metadata)?;
    } else {
        validate_directory_identity(path, &descriptor_metadata)?;
        if descriptor_metadata.mode() & 0o022 != 0 {
            return Err(security_error(
                path,
                io::ErrorKind::PermissionDenied,
                "directory is writable by a group or other user",
            ));
        }
    }
    if path_metadata.dev() != descriptor_metadata.dev()
        || path_metadata.ino() != descriptor_metadata.ino()
    {
        return Err(security_error(
            path,
            io::ErrorKind::Other,
            "directory changed while it was being opened",
        ));
    }

    if descriptor_metadata.mode() & 0o7777 != 0o700 {
        // Use the already-open descriptor so a replacement symlink cannot
        // redirect chmod to an unrelated path.
        directory.set_permissions(Permissions::from_mode(0o700))?;
    }
    let final_metadata = directory.metadata()?;
    validate_directory_metadata(path, &final_metadata, Some(0o700))
}

fn validate_runtime_directory(path: &Path) -> io::Result<()> {
    // XDG_RUNTIME_DIR is defined as an absolute path. Accepting a relative
    // value would make the process working directory part of the allocator's
    // namespace, so two Sommelier instances launched from different
    // directories could reserve the same numeric block through different
    // lock trees.
    if !path.is_absolute() {
        return Err(security_error(
            path,
            io::ErrorKind::InvalidInput,
            "runtime directory must be an absolute path",
        ));
    }
    let path_metadata = fs::symlink_metadata(path)?;
    validate_directory_metadata(path, &path_metadata, None)?;
    let directory = open_directory(path)?;
    let descriptor_metadata = directory.metadata()?;
    validate_directory_metadata(path, &descriptor_metadata, None)?;
    if path_metadata.dev() != descriptor_metadata.dev()
        || path_metadata.ino() != descriptor_metadata.ino()
    {
        return Err(security_error(
            path,
            io::ErrorKind::Other,
            "runtime directory changed while it was being opened",
        ));
    }
    Ok(())
}

fn validate_lock_file(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(security_error(
            path,
            io::ErrorKind::InvalidData,
            "lock path is not a regular file",
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(security_error(
            path,
            io::ErrorKind::PermissionDenied,
            "lock file is not owned by the effective user",
        ));
    }
    if metadata.mode() & 0o7777 != 0o600 {
        return Err(security_error(
            path,
            io::ErrorKind::PermissionDenied,
            "lock file permissions are not exactly 0600",
        ));
    }
    Ok(())
}

fn open_lock_file_at(directory_fd: RawFd, path: &Path, file_name: &str) -> io::Result<File> {
    let c_file_name = CString::new(file_name.as_bytes()).map_err(|_| {
        security_error(
            path,
            io::ErrorKind::InvalidInput,
            "lock file name contains an embedded NUL",
        )
    })?;
    let flags = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let create_flags = flags | libc::O_RDWR | libc::O_CREAT | libc::O_EXCL;
    let fd = unsafe { libc::openat(directory_fd, c_file_name.as_ptr(), create_flags, 0o600) };
    if fd >= 0 {
        // A restrictive umask can remove owner write permission even for a
        // newly-created file. Normalize it through the descriptor, then
        // validate the resulting inode.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() || metadata.uid() != effective_uid() {
            return Err(security_error(
                path,
                io::ErrorKind::PermissionDenied,
                "new lock path did not produce an owned regular file",
            ));
        }
        file.set_permissions(Permissions::from_mode(0o600))?;
        validate_lock_file(path, &file)?;
        return Ok(file);
    }

    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::AlreadyExists {
        return Err(error);
    }

    // Existing inodes are never chmod'ed implicitly. This prevents a stale or
    // attacker-created lock file from being silently trusted.
    let existing_flags = flags | libc::O_RDWR;
    let fd = unsafe { libc::openat(directory_fd, c_file_name.as_ptr(), existing_flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    validate_lock_file(path, &file)?;
    Ok(file)
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
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
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
        fs::set_permissions(&path, Permissions::from_mode(0o700))
            .expect("restrict test lock directory");
        path
    }

    fn first_lock_path(directory: &Path) -> PathBuf {
        let (start_id, end_id) = block_bounds(0);
        directory.join(format!("{start_id}-{end_id}.lock"))
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
    fn block_ranges_are_contiguous_and_cover_the_private_pool() {
        let mut previous_end = ARC_TASK_ID_POOL_START.saturating_sub(1);
        for index in 0..ARC_TASK_ID_BLOCK_COUNT {
            let (start_id, end_id) = block_bounds(index);
            assert_eq!(
                start_id,
                previous_end.saturating_add(1),
                "block {index} must begin after the previous block"
            );
            assert!(start_id <= end_id, "block {index} must not be empty");
            previous_end = end_id;
        }
        assert_eq!(previous_end, ARC_TASK_ID_POOL_END);
    }

    #[test]
    fn oversized_start_index_is_wrapped_before_block_scan() {
        let directory = temporary_directory("start-index-wrap");
        let start_index = u32::MAX;
        let allocator = ArcTaskIdAllocator::acquire_in_directory(&directory, start_index)
            .expect("wrapped start index must reserve a block");
        assert_eq!(
            allocator.block_range(),
            block_bounds(start_index % ARC_TASK_ID_BLOCK_COUNT)
        );
        drop(allocator);
        fs::remove_dir_all(directory).expect("remove test lock directory");
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
    fn max_u32_test_endpoint_reports_exhaustion_without_repeating_id() {
        let allocator = ArcTaskIdAllocator::for_test(u32::MAX, u32::MAX);
        assert_eq!(allocator.allocate().unwrap(), u32::MAX);
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
    fn malformed_first_lock_fails_closed_instead_of_skipping_to_next_block() {
        let directory = temporary_directory("malformed-first-lock");
        let file = File::create(first_lock_path(&directory)).expect("create malformed lock");
        file.set_permissions(Permissions::from_mode(0o644))
            .expect("make lock mode unsafe");
        drop(file);

        let error = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect_err("malformed lock must abort the scan");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn recreated_block_directory_is_rejected_while_old_allocator_is_alive() {
        let parent = temporary_directory("directory-recreate-parent");
        let directory = parent.join("arc-task-blocks");
        fs::create_dir(&directory).expect("create block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict block directory");

        let first = ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
            .expect("first allocator reserves the block");
        fs::remove_dir_all(&directory).expect("remove block directory");
        fs::create_dir(&directory).expect("recreate block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict recreated block directory");

        let error = ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
            .expect_err("a recreated directory must not split the lock namespace");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        drop(first);
        let recovered = ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
            .expect("the marker can be adopted after the old allocator exits");
        drop(recovered);
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn recreated_sommelier_parent_is_rejected_while_old_allocator_is_alive() {
        let runtime = temporary_directory("parent-recreate-runtime");
        let sommelier = runtime.join("sommelier");
        let directory = sommelier.join("arc-task-blocks");
        fs::create_dir(&sommelier).expect("create Sommelier directory");
        fs::set_permissions(&sommelier, Permissions::from_mode(0o700))
            .expect("restrict Sommelier directory");
        fs::create_dir(&directory).expect("create block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict block directory");

        // The production guard lives in the stable runtime directory, so
        // replacing the whole `sommelier/` tree must not create a second
        // generation while the first allocator still holds its shared lock.
        let first = ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0)
            .expect("first allocator reserves the block");
        fs::remove_dir_all(&sommelier).expect("remove Sommelier directory");
        fs::create_dir(&sommelier).expect("recreate Sommelier directory");
        fs::set_permissions(&sommelier, Permissions::from_mode(0o700))
            .expect("restrict recreated Sommelier directory");
        fs::create_dir(&directory).expect("recreate block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict recreated block directory");

        let error = ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0)
            .expect_err("a recreated parent must not split the lock namespace");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        drop(first);
        let recovered =
            ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &runtime, 0)
                .expect("the marker can be adopted after the old allocator exits");
        drop(recovered);
        fs::remove_dir_all(runtime).expect("remove test directories");
    }

    #[test]
    fn malformed_directory_guard_fails_closed() {
        let parent = temporary_directory("malformed-directory-guard-parent");
        let directory = parent.join("arc-task-blocks");
        fs::create_dir(&directory).expect("create block directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .expect("restrict block directory");
        let guard_path = parent.join(DIRECTORY_GUARD_FILE);
        fs::write(&guard_path, b"not-a-directory-identity\n").expect("write malformed guard");
        fs::set_permissions(&guard_path, Permissions::from_mode(0o600))
            .expect("restrict guard file");

        let error = ArcTaskIdAllocator::acquire_in_directory_with_guard(&directory, &parent, 0)
            .expect_err("malformed directory guard must abort allocation");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn concurrent_allocations_are_unique_and_stay_inside_the_reserved_block() {
        let allocator = ArcTaskIdAllocator::for_test(2_000_000_000, 2_000_000_999);
        let mut workers = Vec::new();
        for _ in 0..4 {
            let allocator = Arc::clone(&allocator);
            workers.push(std::thread::spawn(move || {
                (0..250)
                    .map(|_| allocator.allocate().expect("test block has capacity"))
                    .collect::<Vec<_>>()
            }));
        }

        let mut ids = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("allocation worker must finish"))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids.len(), 1_000);
        assert!(
            ids.windows(2).all(|pair| pair[0] + 1 == pair[1]),
            "concurrent allocation must not duplicate or skip IDs"
        );
        assert_eq!(ids.first().copied(), Some(2_000_000_000));
        assert_eq!(ids.last().copied(), Some(2_000_000_999));
    }

    #[test]
    fn newly_created_lock_directory_is_normalized_to_private_mode() {
        let parent = temporary_directory("directory-mode-parent");
        let directory = parent.join("new-private-directory");
        ensure_private_directory(&directory).expect("create private lock directory");
        assert_eq!(
            fs::symlink_metadata(&directory)
                .expect("inspect private lock directory")
                .mode()
                & 0o7777,
            0o700
        );
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn existing_umask_directory_is_tightened_without_following_a_symlink() {
        let parent = temporary_directory("existing-directory-mode-parent");
        let directory = parent.join("existing-directory");
        fs::create_dir(&directory).expect("create existing lock directory");
        fs::set_permissions(&directory, Permissions::from_mode(0o755))
            .expect("set legacy lock directory mode");

        ensure_private_directory(&directory).expect("tighten legacy lock directory");
        assert_eq!(
            fs::symlink_metadata(&directory)
                .expect("inspect tightened lock directory")
                .mode()
                & 0o7777,
            0o700
        );
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn symlink_lock_path_is_rejected() {
        let directory = temporary_directory("symlink-lock");
        let target = directory.join("target");
        let target_file = File::create(&target).expect("create symlink target");
        target_file
            .set_permissions(Permissions::from_mode(0o600))
            .expect("restrict symlink target");
        symlink(&target, first_lock_path(&directory)).expect("create lock symlink");

        let error = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect_err("symlink lock path must be rejected");
        assert!(
            matches!(
                error.raw_os_error(),
                Some(code) if code == libc::ELOOP
            ) || error.kind() == io::ErrorKind::PermissionDenied
        );
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn non_regular_lock_path_is_rejected() {
        let directory = temporary_directory("non-regular-lock");
        fs::create_dir(first_lock_path(&directory)).expect("create directory lock path");

        let error = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect_err("directory lock path must be rejected");
        assert!(
            matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EISDIR
            ) || error.kind() == io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn fifo_lock_path_is_rejected_without_blocking() {
        let directory = temporary_directory("fifo-lock");
        let path = first_lock_path(&directory);
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("lock path has no NUL");
        assert_eq!(
            unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) },
            0,
            "create FIFO lock path"
        );

        let error = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect_err("FIFO lock path must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn existing_lock_with_unsafe_mode_is_rejected() {
        let directory = temporary_directory("unsafe-lock-mode");
        let file = File::create(first_lock_path(&directory)).expect("create lock file");
        file.set_permissions(Permissions::from_mode(0o644))
            .expect("make lock mode unsafe");
        drop(file);

        let error = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect_err("unsafe lock mode must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn existing_owned_0600_lock_file_remains_usable() {
        let directory = temporary_directory("valid-existing-lock");
        let file = File::create(first_lock_path(&directory)).expect("create lock file");
        file.set_permissions(Permissions::from_mode(0o600))
            .expect("restrict lock file");
        drop(file);

        let allocator = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect("valid existing lock file remains usable");
        assert_eq!(allocator.block_range(), block_bounds(0));
        drop(allocator);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn symlink_lock_directory_is_rejected() {
        let parent = temporary_directory("symlink-directory-parent");
        let target = parent.join("target");
        fs::create_dir(&target).expect("create lock directory target");
        fs::set_permissions(&target, Permissions::from_mode(0o700))
            .expect("restrict lock directory target");
        let link = parent.join("link");
        symlink(&target, &link).expect("create lock directory symlink");

        let error = ArcTaskIdAllocator::acquire_in_directory(&link, 0)
            .expect_err("symlink lock directory must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        fs::remove_dir_all(parent).expect("remove test directories");
    }

    #[test]
    fn lock_directory_with_unsafe_mode_is_rejected() {
        let directory = temporary_directory("unsafe-directory-mode");
        fs::set_permissions(&directory, Permissions::from_mode(0o777))
            .expect("make lock directory mode unsafe");

        let error = ArcTaskIdAllocator::acquire_in_directory(&directory, 0)
            .expect_err("unsafe lock directory mode must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir_all(directory).expect("remove test lock directory");
    }

    #[test]
    fn relative_runtime_directory_is_rejected_before_filesystem_access() {
        let error = validate_runtime_directory(Path::new("relative-runtime"))
            .expect_err("allocator runtime directory must be absolute");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("runtime directory must be an absolute path"),
            "error should explain the XDG path contract: {error}"
        );
    }
}
