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

//! Reloading the bindings file as it changes: an inotify instance on the
//! proxy's event loop, with no thread, no timer, and no I/O on the key
//! path.
//!
//! The file's path may lead through symbolic links, a link of the file's
//! own or of a directory on its path, and each link adds a path: the
//! target's, with the rest of the path below it ([`chain`]). On each path
//! of that chain, [`Watcher`] watches what can change the file the
//! configured path names:
//!
//! * the file, through links: an edit in place closes it written or
//!   changes its attributes;
//! * the deepest directory on the path that exists, for the entry below
//!   it on the path: an editor's or a dotfile tool's rename over the file,
//!   a Home Manager activation's swap of a link, a removal, and a missing
//!   directory, file or link target being created all change that entry;
//! * that directory's parent, for the directory's own entry, which an
//!   activation can swap as a whole link.
//!
//! And each link of the chain is watched in the directory that holds it,
//! for its own entry, which an activation swaps, even a directory the
//! proxy cannot write, as NixOS's `/etc` holds `/etc/static`.
//!
//! Only the directory that holds the file reports writes; the others
//! report only their entries' changes, so writes elsewhere, such as a
//! shell's history in the home directory, do not wake the proxy. A
//! directory the proxy cannot write that holds no link of the chain, such
//! as the Nix store, which holds the file a chain ends at, is not watched
//! on a link's path: nothing in it changes but by the system's hand, and
//! a swap of the link into it is seen where the link is.
//!
//! Each batch of events that concerns a path arms the watches again, as
//! the inodes behind the paths may have changed, and reloads the file
//! ([`BindingsFile::reload`]). The reload's identity check makes the many
//! events of one change load the file once and warn once. A regular file
//! that has just been created is not read until its writer closes it or
//! it is renamed into place, so a half-written file is not reported.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use nix::errno::Errno;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use tokio::io::unix::AsyncFd;

use super::{BindingsFile, PlacementConfig, Refresh};
use crate::settings::Settings;

/// What a watch on the file reports: a write closed, a change of its
/// attributes or links, and its end.
const FILE_EVENTS: AddWatchFlags = AddWatchFlags::IN_CLOSE_WRITE
    .union(AddWatchFlags::IN_ATTRIB)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_MOVE_SELF);

/// What a watch on a directory on a path reports: its entries' changes,
/// and its own end.
const ENTRY_EVENTS: AddWatchFlags = AddWatchFlags::IN_CREATE
    .union(AddWatchFlags::IN_DELETE)
    .union(AddWatchFlags::IN_MOVED_FROM)
    .union(AddWatchFlags::IN_MOVED_TO)
    .union(AddWatchFlags::IN_DELETE_SELF)
    .union(AddWatchFlags::IN_MOVE_SELF)
    .union(AddWatchFlags::IN_ONLYDIR);

/// What a watch on the directory that holds the file reports besides:
/// writes closed and attribute changes, which the file's own watch misses
/// while the file is being created.
const HOLDER_EVENTS: AddWatchFlags = ENTRY_EVENTS
    .union(AddWatchFlags::IN_CLOSE_WRITE)
    .union(AddWatchFlags::IN_ATTRIB);

/// Events that say a watch ended or lost events, whatever it watched.
const WATCH_ENDED: AddWatchFlags = AddWatchFlags::IN_DELETE_SELF
    .union(AddWatchFlags::IN_MOVE_SELF)
    .union(AddWatchFlags::IN_IGNORED);

/// How many links the chain follows, as the kernel bounds a lookup.
const MAX_LINKS: usize = 40;

/// How often arming starts over because a directory on a path appeared
/// while it armed.
const MAX_ARMS: usize = 8;

/// Load the bindings file the settings name, if any, and reload it as it
/// changes, on a task of the current `LocalSet`.
///
/// # Returns
///
/// The configuration every connection shares; off for good when the
/// settings name no file.
pub(crate) fn load_and_watch(settings: &Rc<Settings>) -> PlacementConfig {
    let config = PlacementConfig::default();
    let Some(mut file) = BindingsFile::new(Rc::clone(settings), config.clone()) else {
        return config;
    };
    // Armed before the first load, so that a change in between is seen.
    let watcher = Watcher::new(file.path())
        .inspect_err(|error| {
            log::warn!(
                "Cannot watch {} for changes: {error}. Edits take effect when the proxy \
                 restarts.",
                file.path().display()
            );
        })
        .ok();
    file.reload();
    if let Some(watcher) = watcher {
        tokio::task::spawn_local(watcher.run(file));
    }
    config
}

/// The paths a configured path leads to, and the links that lead there.
#[derive(Debug, PartialEq)]
struct Chain {
    /// The configured path, then each path a symbolic link on the one
    /// before leads to: the link's target, with the rest of the path below
    /// the link.
    paths: Vec<PathBuf>,
    /// The link each path but the first goes through, in order.
    links: Vec<PathBuf>,
}

/// The [`Chain`] of `path`. At most [`MAX_LINKS`] links are followed.
fn chain(path: &Path) -> Chain {
    let mut chain = Chain {
        paths: vec![path.to_path_buf()],
        links: Vec::new(),
    };
    let mut last = path.to_path_buf();
    while chain.links.len() < MAX_LINKS {
        match through_first_link(&last) {
            Some((link, next)) if !chain.paths.contains(&next) => {
                chain.links.push(link);
                chain.paths.push(next.clone());
                last = next;
            }
            _ => break,
        }
    }
    chain
}

/// `path` through its first symbolic link, counted from the root, if it
/// has one that can be read: the link, and the path through it.
fn through_first_link(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut link = PathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        link.push(component);
        if std::fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.is_symlink()) {
            let target = std::fs::read_link(&link).ok()?;
            // No link precedes this one, so its directory holds none, and
            // a `..` of the target can go up from it by name.
            let directory = link.parent().unwrap_or(Path::new("/"));
            let next = lexical(&directory.join(target).join(components.as_path()));
            return Some((link, next));
        }
    }
    None
}

/// `path` with its `.` and `..` components resolved by name.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    resolved.push(component);
                }
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// What one inotify watch is for.
#[derive(Debug)]
enum Watch {
    /// The file a path names, through symbolic links.
    File,
    /// A directory on the paths, for these entries of it, each the next
    /// on a path.
    Directory(Vec<Entry>),
}

/// An entry a directory watch is for: its name in the directory, and its
/// path.
#[derive(Debug)]
struct Entry {
    name: OsString,
    path: PathBuf,
}

/// The watches one arming makes, with the events each reports.
#[derive(Default)]
struct Armed {
    watches: BTreeMap<WatchDescriptor, (AddWatchFlags, Watch)>,
    directories: usize,
    /// A path exchanged its inode while this armed, so a watch may name
    /// an inode the path no longer leads to.
    changed: bool,
    /// A failure other than a missing or unwatchable directory.
    error: Option<Errno>,
}

/// What [`Watcher::add`] made of a watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Added {
    /// A watch new to the arming.
    New,
    /// The arming's watch of the same inode, which now reports these
    /// events too.
    Joined,
    /// No watch: the path is missing, unreachable or unreadable.
    Missing,
}

impl Added {
    fn watched(self) -> bool {
        self != Self::Missing
    }
}

/// The inotify instance, as `AsyncFd` takes it.
struct InotifyFd(Inotify);

impl AsRawFd for InotifyFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_fd().as_raw_fd()
    }
}

/// The watches on the bindings file's paths; see the module docs.
pub(super) struct Watcher {
    inotify: AsyncFd<InotifyFd>,
    path: PathBuf,
    watches: BTreeMap<WatchDescriptor, Watch>,
    /// Regular files created empty whose writer has not closed them yet.
    being_written: BTreeSet<PathBuf>,
    /// Whether a watch failure, such as the watch limit, was warned about.
    failure_warned: bool,
}

impl Watcher {
    /// Watch the file at `path`, which need not exist, nor its directory.
    ///
    /// # Errors
    ///
    /// The inotify instance cannot be made, or no directory on the paths
    /// can be watched.
    pub(super) fn new(path: &Path) -> io::Result<Self> {
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
        let mut watcher = Self {
            inotify: AsyncFd::new(InotifyFd(inotify))?,
            path: path.to_path_buf(),
            watches: BTreeMap::new(),
            being_written: BTreeSet::new(),
            failure_warned: false,
        };
        watcher.arm()?;
        Ok(watcher)
    }

    /// Reload `file` whenever its paths change, until the watches fail.
    async fn run(mut self, mut file: BindingsFile) {
        loop {
            if let Err(error) = self.step(&mut file).await {
                log::warn!(
                    "Stopped watching {}: {error}. Edits take effect when the proxy restarts.",
                    file.path().display()
                );
                return;
            }
        }
    }

    /// Wait for the next batch of events, and reload `file` if it concerns
    /// a path.
    ///
    /// # Returns
    ///
    /// What the reload did, or `None` when the batch did not reload.
    ///
    /// # Errors
    ///
    /// Reading the events failed, or arming again left no directory
    /// watched.
    pub(super) async fn step(&mut self, file: &mut BindingsFile) -> io::Result<Option<Refresh>> {
        Ok(self.next().await?.then(|| file.reload()))
    }

    /// Wait for the next batch of events, arm the watches again if it
    /// concerns a path, and return whether the file must be reloaded.
    async fn next(&mut self) -> io::Result<bool> {
        let events = loop {
            let mut ready = self.inotify.readable().await?;
            if let Ok(events) =
                ready.try_io(|inotify| inotify.get_ref().0.read_events().map_err(io::Error::from))
            {
                break events?;
            }
        };
        let mut changed = false;
        for event in &events {
            if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                self.being_written.clear();
                changed = true;
                continue;
            }
            match self.watches.get(&event.wd) {
                Some(Watch::File) => changed = true,
                Some(Watch::Directory(entries)) => {
                    if event.mask.intersects(WATCH_ENDED) {
                        changed = true;
                    }
                    let Some(entry) = entries
                        .iter()
                        .find(|entry| event.name.as_ref() == Some(&entry.name))
                    else {
                        continue;
                    };
                    changed = true;
                    if event.mask.contains(AddWatchFlags::IN_CREATE) {
                        // An empty regular file being created is read once
                        // its writer closes it, or it is renamed into place.
                        if std::fs::symlink_metadata(&entry.path)
                            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == 0)
                        {
                            self.being_written.insert(entry.path.clone());
                        }
                    } else if !event.mask.contains(AddWatchFlags::IN_ATTRIB) {
                        self.being_written.remove(&entry.path);
                    }
                }
                // A watch this instance already removed.
                None => {}
            }
        }
        if changed {
            self.arm()?;
        }
        Ok(changed && self.being_written.is_empty())
    }

    /// Watch, on every path of the [`chain`] as it is now, the file, the
    /// deepest directory that exists and that directory's parent, and each
    /// link of the chain where it is, and nothing else. A directory on a
    /// path that appears while this arms, or a path whose inode changes
    /// while it is watched, makes it start over; the watches of an attempt
    /// that starts over are removed unless the last one keeps them.
    ///
    /// # Errors
    ///
    /// A watch failed, other than for a missing or unwatchable directory,
    /// and no directory is watched: then no change could be seen. Each
    /// such failure is logged once.
    fn arm(&mut self) -> io::Result<()> {
        // Every watch this instance may hold: the last arming's, and each
        // attempt's of this one.
        let mut made: BTreeSet<WatchDescriptor> = self.watches.keys().copied().collect();
        let mut armed = Armed::default();
        for _ in 0..MAX_ARMS {
            made.extend(armed.watches.keys().copied());
            armed = Armed::default();
            let chain = chain(&self.path);
            let mut settled = true;
            for (hop, path) in chain.paths.iter().enumerate() {
                settled &= self.arm_path(&mut armed, path, hop > 0);
            }
            for link in &chain.links {
                self.arm_link(&mut armed, link);
            }
            if settled && !armed.changed {
                break;
            }
        }
        if let Some(error) = armed.error {
            let level = if std::mem::replace(&mut self.failure_warned, true) {
                log::Level::Debug
            } else {
                log::Level::Warn
            };
            log::log!(
                level,
                "Cannot watch every directory on the path of {}: {error}",
                self.path.display()
            );
            if armed.directories == 0 {
                return Err(error.into());
            }
        }
        let inotify = &self.inotify.get_ref().0;
        for watch in made {
            if !armed.watches.contains_key(&watch) {
                // The inode may be gone, which removed the watch already.
                let _ = inotify.rm_watch(watch);
            }
        }
        self.watches = armed
            .watches
            .into_iter()
            .map(|(watch, (_, watch_for))| (watch, watch_for))
            .collect();
        self.being_written.retain(|path| path.exists());
        Ok(())
    }

    /// Watch `path`'s file, the deepest directory on it that exists and
    /// that directory's parent into `armed`. On a link's path (`linked`),
    /// a directory the proxy cannot write is not watched.
    ///
    /// # Returns
    ///
    /// Whether the watches are settled: `false` when the entry below the
    /// deepest watched directory appeared while this armed, so that the
    /// watch that would see it changing was not made.
    fn arm_path(&self, armed: &mut Armed, path: &Path, linked: bool) -> bool {
        let file = self.add(armed, path, FILE_EVENTS, Watch::File);
        let file_watched = file.is_ok_and(Added::watched);
        let mut entry = path;
        let mut directories = 0;
        let mut settled = true;
        while let (Some(directory), Some(name)) = (entry.parent(), entry.file_name()) {
            if linked
                && matches!(
                    nix::unistd::access(directory, nix::unistd::AccessFlags::W_OK),
                    Err(Errno::EACCES | Errno::EROFS)
                )
            {
                break;
            }
            let events = if entry == path {
                HOLDER_EVENTS
            } else {
                ENTRY_EVENTS
            };
            let watch_for = Watch::Directory(vec![Entry {
                name: name.to_os_string(),
                path: entry.to_path_buf(),
            }]);
            match self.add(armed, directory, events, watch_for) {
                Ok(Added::New | Added::Joined) => {}
                // Missing, or no directory: try the one above it.
                Ok(Added::Missing) if directories == 0 => {
                    entry = directory;
                    continue;
                }
                _ => break,
            }
            if directories == 0 {
                // The entry's own watch failed while it was missing: if it
                // is here now, it came in between.
                settled = if entry == path {
                    file_watched || std::fs::metadata(entry).is_err()
                } else {
                    !std::fs::metadata(entry).is_ok_and(|metadata| metadata.is_dir())
                };
            }
            directories += 1;
            if directories == 2 {
                break;
            }
            entry = directory;
        }
        armed.directories += directories;
        settled
    }

    /// Watch the directory that holds `link`, a link of the chain, for
    /// the link's entry into `armed`, whether or not the proxy can write
    /// the directory: a swap of the link changes the entry.
    fn arm_link(&self, armed: &mut Armed, link: &Path) {
        let (Some(directory), Some(name)) = (link.parent(), link.file_name()) else {
            return;
        };
        let watch_for = Watch::Directory(vec![Entry {
            name: name.to_os_string(),
            path: link.to_path_buf(),
        }]);
        if self.add(armed, directory, ENTRY_EVENTS, watch_for) == Ok(Added::New) {
            armed.directories += 1;
        }
    }

    /// Watch `path` for `events` as `watch_for`, joining a watch of the
    /// same inode that `armed` holds.
    ///
    /// A join adds its events to the watch's (`IN_MASK_ADD`), so the watch
    /// never loses one this arming asked for, even for an instant; a watch
    /// new to this arming takes exactly its events, which drops those an
    /// earlier arming asked of the inode.
    ///
    /// A watch new to the arming is made twice: the first names the inode
    /// the watch is for, and the second sets its events, since only a new
    /// watch can drop events. If the path's inode changed between them, the
    /// second watch names another inode, which this arming does not keep:
    /// it is removed, and `armed` records that the arming must start over.
    ///
    /// # Returns
    ///
    /// Whether the watch was made, new or joined, or not, for a missing,
    /// unreachable or unreadable path.
    ///
    /// # Errors
    ///
    /// Any other failure, such as the watch limit, which is also recorded
    /// in `armed`.
    fn add(
        &self,
        armed: &mut Armed,
        path: &Path,
        events: AddWatchFlags,
        watch_for: Watch,
    ) -> Result<Added, Errno> {
        let inotify = &self.inotify.get_ref().0;
        // `nix` names no `IN_MASK_ADD`.
        let mask_add = AddWatchFlags::from_bits_retain(libc::IN_MASK_ADD);
        let joining = events | mask_add;
        let added = inotify.add_watch(path, joining).and_then(|watch| {
            match armed.watches.get_mut(&watch) {
                Some((mask, existing)) => {
                    *mask |= events;
                    if let (Watch::Directory(entries), Watch::Directory(more)) =
                        (existing, watch_for)
                    {
                        entries.extend(more);
                    }
                    Ok(Added::Joined)
                }
                None => {
                    let second = inotify.add_watch(path, events)?;
                    if second != watch {
                        // The path's inode changed between the two watches,
                        // so the second one names an inode this arming does
                        // not keep; remove it rather than leak it, and arm
                        // again where the path leads now.
                        let _ = inotify.rm_watch(second);
                        armed.changed = true;
                    }
                    armed.watches.insert(watch, (events, watch_for));
                    Ok(Added::New)
                }
            }
        });
        match added {
            Ok(added) => Ok(added),
            Err(Errno::ENOENT | Errno::ENOTDIR | Errno::EACCES | Errno::ELOOP) => {
                Ok(Added::Missing)
            }
            Err(error) => {
                armed.error = Some(error);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{alt_q, half, one_binding, preset, replace, store_file, swap_link};
    use super::*;
    use crate::test_support::TempDir;
    use std::time::Duration;

    /// The next reload `watcher` makes of `file`, which the operations
    /// before have caused; batches that do not concern the path, which
    /// other tests' directories can cause, are skipped.
    async fn reload(watcher: &mut Watcher, file: &mut BindingsFile) -> Refresh {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(refresh) = watcher.step(file).await.expect("read inotify events") {
                    return refresh;
                }
            }
        })
        .await
        .expect("the change reached the watcher")
    }

    /// The bindings file under the configuration directory `config_home`,
    /// its watcher, and the configuration it publishes to, loaded once.
    fn watched(config_home: &Path) -> (Watcher, BindingsFile, PlacementConfig) {
        let path = config_home.join("sommelier-rs/window-placement.toml");
        let watcher = Watcher::new(&path).expect("watch the bindings file");
        let settings = Settings {
            placement_bindings: Some(path),
            ..Settings::for_test()
        };
        let config = PlacementConfig::default();
        let mut file = BindingsFile::new(Rc::new(settings), config.clone()).unwrap();
        file.reload();
        (watcher, file, config)
    }

    fn bind_alt_q(action: &str) -> String {
        one_binding(&format!("action = \"{action}\""))
    }

    /// Assert that every watch the kernel holds for `watcher`'s inotify
    /// instance is one the watcher keeps, and the reverse.
    fn assert_no_stray_watch(watcher: &Watcher) {
        let fd = watcher.inotify.get_ref().as_raw_fd();
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
        let held = info.lines().filter(|line| line.starts_with("inotify wd:"));
        assert_eq!(held.count(), watcher.watches.len(), "{info}");
    }

    /// Every way a file comes, changes and goes reaches the bindings with
    /// no key press: the configuration directory and the file created
    /// later, an edit in place, a rename over the file, a bad edit (which
    /// keeps the last good bindings), Home Manager's swap of a link into
    /// its store, an edit of the file a link names, and a removal.
    #[tokio::test]
    async fn the_watcher_follows_every_change_of_the_file() {
        let dir = TempDir::new("watch-file");
        let config_home = dir.path().join("xdg");
        let (mut watcher, mut file, config) = watched(&config_home);
        assert!(!config.is_active());

        std::fs::create_dir_all(config_home.join("sommelier-rs")).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Unchanged);
        let path = config_home.join("sommelier-rs/window-placement.toml");
        std::fs::write(&path, bind_alt_q("left")).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("left"));

        // In place: the same inode.
        std::fs::write(&path, bind_alt_q("right")).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("right"));

        let directory = config_home.join("sommelier-rs");
        let in_directory = |name: &str, contents: &str| {
            let staged = directory.join(".staged");
            std::fs::write(&staged, contents).unwrap();
            std::fs::rename(staged, directory.join(name)).unwrap();
        };
        in_directory("window-placement.toml", &bind_alt_q("top"));
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("top"));

        in_directory("window-placement.toml", "not toml");
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Rejected);
        assert_eq!(alt_q(&config), preset("top"), "the last good bindings");

        let left = store_file(&dir, "left.toml", "0.0");
        let right = store_file(&dir, "right.toml", "0.5");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&left, &link).unwrap();
        std::fs::rename(&link, &path).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), half(0.0));
        swap_link(&dir, &path, &right);
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), half(0.5));

        // The linked file, outside every watched directory, in place.
        std::fs::write(&right, bind_alt_q("bottom")).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("bottom"));

        std::fs::remove_file(&path).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Absent);
        assert!(!config.is_active());
    }

    /// Home Manager can link the whole configuration directory into its
    /// store; an activation swaps that link.
    #[tokio::test]
    async fn the_watcher_follows_a_swap_of_the_directory_link() {
        let dir = TempDir::new("watch-directory");
        let store = |name: &str, action: &str| {
            let directory = dir.path().join(name);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("window-placement.toml"), bind_alt_q(action)).unwrap();
            directory
        };
        let (old, new) = (store("old", "left"), store("new", "right"));
        let config_home = dir.path().join("xdg");
        std::fs::create_dir(&config_home).unwrap();
        std::os::unix::fs::symlink(&old, config_home.join("sommelier-rs")).unwrap();
        let (mut watcher, mut file, config) = watched(&config_home);
        assert_eq!(alt_q(&config), preset("left"));

        swap_link(&dir, &config_home.join("sommelier-rs"), &new);
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("right"));
    }

    /// A file being written is read once its writer closes it, so its
    /// empty first state is not reported as invalid.
    #[tokio::test]
    async fn a_new_file_is_read_once_written() {
        let dir = TempDir::new("watch-new");
        std::fs::create_dir(dir.path().join("sommelier-rs")).unwrap();
        let (mut watcher, mut file, config) = watched(dir.path());
        let path = dir.path().join("sommelier-rs/window-placement.toml");

        let mut writer = std::fs::File::create(&path).unwrap();
        assert_eq!(
            watcher.step(&mut file).await.unwrap(),
            None,
            "being written"
        );
        std::io::Write::write_all(&mut writer, bind_alt_q("left").as_bytes()).unwrap();
        drop(writer);
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("left"));
    }

    /// A file being written stays unread even when another event of the
    /// same batch, here a change of its mode, concerns the path.
    #[tokio::test]
    async fn a_file_being_written_is_not_read_for_another_event() {
        let dir = TempDir::new("watch-writing");
        std::fs::create_dir(dir.path().join("sommelier-rs")).unwrap();
        let (mut watcher, mut file, config) = watched(dir.path());
        let path = dir.path().join("sommelier-rs/window-placement.toml");

        let mut writer = std::fs::File::create(&path).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o600);
        std::fs::set_permissions(&path, mode).unwrap();
        assert_eq!(watcher.step(&mut file).await.unwrap(), None);
        std::io::Write::write_all(&mut writer, bind_alt_q("left").as_bytes()).unwrap();
        drop(writer);
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("left"));
    }

    /// A bindings file that links into a dotfiles directory follows that
    /// directory too: the target deleted and created anew (as `rm` and
    /// `cp` do, or an editor's first save), a link that dangles until its
    /// target is created, and a link through a directory link that is
    /// swapped to another version.
    #[tokio::test]
    async fn the_watcher_follows_the_target_of_a_link() {
        let dir = TempDir::new("watch-target");
        let dotfiles = dir.path().join("dotfiles");
        let target = dotfiles.join("window-placement.toml");
        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::create_dir(dir.path().join("sommelier-rs")).unwrap();
        let path = dir.path().join("sommelier-rs/window-placement.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        // Dangling at startup.
        let (mut watcher, mut file, config) = watched(dir.path());
        assert!(!config.is_active());
        std::fs::write(&target, bind_alt_q("left")).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("left"));

        // Deleted, then created anew.
        std::fs::remove_file(&target).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Absent);
        std::fs::write(&target, bind_alt_q("right")).unwrap();
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("right"));

        // Through `current`, a link to one version of the dotfiles.
        for (version, action) in [("v1", "top"), ("v2", "bottom")] {
            let version = dotfiles.join(version);
            std::fs::create_dir(&version).unwrap();
            std::fs::write(version.join("window-placement.toml"), bind_alt_q(action)).unwrap();
        }
        std::os::unix::fs::symlink(dotfiles.join("v1"), dotfiles.join("current")).unwrap();
        swap_link(&dir, &path, &dotfiles.join("current/window-placement.toml"));
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("top"));
        assert_no_stray_watch(&watcher);
        swap_link(&dir, &dotfiles.join("current"), &dotfiles.join("v2"));
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("bottom"));
        assert_no_stray_watch(&watcher);
    }

    /// A link of the chain in a directory the proxy cannot write, as
    /// NixOS's `/etc` holds `/etc/static`, is watched where it is: the
    /// system's swap of it reaches the bindings. The store it links into,
    /// which holds only the file, is not watched.
    #[tokio::test]
    async fn the_watcher_follows_a_link_in_a_directory_it_cannot_write() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = TempDir::new("watch-read-only-link");
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        for (name, action) in [("old.toml", "left"), ("new.toml", "right")] {
            std::fs::write(store.join(name), bind_alt_q(action)).unwrap();
        }
        let etc = dir.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        let link = etc.join("window-placement.toml");
        symlink(store.join("old.toml"), &link).unwrap();
        std::fs::create_dir(dir.path().join("sommelier-rs")).unwrap();
        symlink(&link, dir.path().join("sommelier-rs/window-placement.toml")).unwrap();
        mode(&store, 0o555);
        mode(&etc, 0o555);
        // Writable again as the test ends, so that its directory can be
        // removed.
        struct Writable(Vec<PathBuf>);
        impl Drop for Writable {
            fn drop(&mut self) {
                for path in &self.0 {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
        let _writable = Writable(vec![store.clone(), etc.clone()]);
        let (mut watcher, mut file, config) = watched(dir.path());
        assert_eq!(alt_q(&config), preset("left"));
        assert_no_stray_watch(&watcher);

        // The system swaps the link.
        mode(&etc, 0o755);
        swap_link(&dir, &link, &store.join("new.toml"));
        mode(&etc, 0o555);
        assert_eq!(reload(&mut watcher, &mut file).await, Refresh::Loaded);
        assert_eq!(alt_q(&config), preset("right"));
        assert_no_stray_watch(&watcher);
    }

    /// The chain of a path: each link, of a directory or the file, adds
    /// the path through its target, resolved from the link's directory.
    #[test]
    fn a_chain_follows_each_link_on_the_path() {
        let dir = TempDir::new("watch-chain");
        // No link above the test's own directory.
        let root = &std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("real/v1")).unwrap();
        std::os::unix::fs::symlink("real", root.join("config")).unwrap();
        std::os::unix::fs::symlink("v1/file", root.join("real/link")).unwrap();
        assert_eq!(
            chain(&root.join("config/link")),
            Chain {
                paths: vec![
                    root.join("config/link"),
                    root.join("real/link"),
                    root.join("real/v1/file"),
                ],
                links: vec![root.join("config"), root.join("real/link")],
            }
        );
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        let loop_chain = chain(&root.join("loop"));
        assert_eq!(loop_chain.paths, [root.join("loop")]);
    }

    /// The watch runs as a task of the event loop: an edit reaches the
    /// shared configuration while the loop runs, with no one asking.
    #[tokio::test]
    async fn load_and_watch_reloads_on_the_event_loop() {
        let dir = TempDir::new("watch-loop");
        let path = replace(&dir, "window-placement.toml", &bind_alt_q("left"));
        tokio::task::LocalSet::new()
            .run_until(async {
                let settings = Settings {
                    placement_bindings: Some(path.clone()),
                    ..Settings::for_test()
                };
                let config = load_and_watch(&Rc::new(settings));
                assert_eq!(alt_q(&config), preset("left"), "loaded at once");
                replace(&dir, "window-placement.toml", &bind_alt_q("right"));
                tokio::time::timeout(Duration::from_secs(5), async {
                    while alt_q(&config) != preset("right") {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the watch task reloaded the file");
            })
            .await;
        assert!(!load_and_watch(&Rc::new(Settings::for_test())).is_active());
    }
}
