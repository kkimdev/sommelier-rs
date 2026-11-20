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

//! Window placement configuration, shared by the whole process.
//!
//! [`PlacementConfig`] is the single owner of every placement setting:
//!
//! * the host accelerators from `SOMMELIER_ACCELERATORS`, read once;
//! * the placement bindings, from one file,
//!   `$XDG_CONFIG_HOME/sommelier-rs/window-placement.toml`: where it is
//!   ([`config_path`], resolved once), when it changed
//!   ([`PlacementConfig::refresh`]), and what it binds (parse and
//!   validation).
//!
//! `main` builds it once with [`PlacementConfig::from_environment`] and
//! every connection's `Context` shares it, so no other code reads these
//! settings and every warning is logged once per process. There are no
//! built-in bindings: placement is on exactly while the file holds a valid
//! configuration.
//!
//! The configuration contract (location, failure policy, reload,
//! precision) is specified once, in
//! `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Deserialize;
use thiserror::Error;

use crate::accelerator::{parse_accelerator, parse_accelerators, Accelerator, ParseError};
use crate::window_layout::{NormalizedRect, RectError, WindowLayoutAction};

/// Keys the host compositor handles.
const HOST_ACCELERATORS_VARIABLE: &str = "SOMMELIER_ACCELERATORS";
/// The bindings file, relative to the XDG configuration directory.
const CONFIG_FILE: &str = "sommelier-rs/window-placement.toml";

/// The only schema version this build understands.
const CONFIG_VERSION: i64 = 1;
/// Upper bound on the file size read from disk. Real configurations are a few
/// kilobytes; the bound keeps a mistaken file from stalling the proxy.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// Upper bound on the number of bindings.
const MAX_BINDINGS: usize = 256;

/// One validated placement binding: a chord the compositor consumes and the
/// action it performs. Only the validator constructs one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PlacementBinding {
    chord: Accelerator,
    action: WindowLayoutAction,
}

/// Why the bindings file was rejected. Indices count bindings from zero.
#[derive(Debug, Error)]
pub(crate) enum BindingsError {
    #[error("cannot open it: {0}")]
    Open(#[source] io::Error),
    #[error("it is not a regular file")]
    NotRegularFile,
    #[error("cannot read it: {0}")]
    Read(#[source] io::Error),
    #[error("it exceeds the {MAX_CONFIG_BYTES}-byte size limit")]
    TooLarge,
    #[error("it is not valid UTF-8")]
    NotUtf8,
    #[error("malformed TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("unsupported version {found}; this build understands version {CONFIG_VERSION}")]
    UnsupportedVersion { found: i64 },
    #[error("{count} bindings exceed the limit of {MAX_BINDINGS}")]
    TooManyBindings { count: usize },
    #[error("bindings[{index}]: invalid chord {chord:?}: {source}")]
    InvalidChord {
        index: usize,
        chord: String,
        #[source]
        source: ParseError,
    },
    #[error("bindings[{index}]: chord {chord:?} duplicates bindings[{first}]")]
    DuplicateChord {
        index: usize,
        first: usize,
        chord: String,
    },
    #[error("bindings[{index}]: chord {chord:?} conflicts with {HOST_ACCELERATORS_VARIABLE}")]
    HostAcceleratorConflict { index: usize, chord: String },
    #[error("bindings[{index}]: unsupported action {action:?}")]
    UnknownAction { index: usize, action: String },
    #[error("bindings[{index}]: exactly one of `action` or `rect` is required")]
    AmbiguousTarget { index: usize },
    #[error("bindings[{index}]: invalid rect {rect:?}: {source}")]
    InvalidRect {
        index: usize,
        rect: [f64; 4],
        #[source]
        source: RectError,
    },
}

/// Where the bindings file is: `$XDG_CONFIG_HOME/sommelier-rs/window-placement.toml`,
/// or under `$HOME/.config` when `XDG_CONFIG_HOME` is unset.
///
/// As the XDG base directory specification asks, an empty or relative
/// `XDG_CONFIG_HOME` counts as unset. A relative `HOME` is not used either,
/// because the path would then depend on the startup directory.
///
/// # Returns
///
/// `None` when neither variable names an absolute directory.
fn config_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    fn absolute(value: Option<&OsStr>) -> Option<&Path> {
        value.map(Path::new).filter(|path| path.is_absolute())
    }
    let config_home = match absolute(xdg_config_home) {
        Some(config_home) => config_home.to_path_buf(),
        None => absolute(home)?.join(".config"),
    };
    Some(config_home.join(CONFIG_FILE))
}

/// Parse `SOMMELIER_ACCELERATORS`; unset or blank is an empty list.
///
/// A malformed list must not stop the proxy: it degrades to no filtering,
/// logged once, and so binds no chord out of reach of a binding either.
fn host_accelerators(value: Option<&OsStr>) -> Vec<Accelerator> {
    let Some(value) = value else {
        return Vec::new();
    };
    value
        .to_str()
        .ok_or_else(|| "it is not UTF-8".to_string())
        .and_then(|text| parse_accelerators(text).map_err(|error| format!("{text:?}: {error}")))
        .unwrap_or_else(|error| {
            log::warn!(
                "Invalid {HOST_ACCELERATORS_VARIABLE}, {error}. Accelerator filtering is disabled."
            );
            Vec::new()
        })
}

/// What identifies one version of the bindings file, as `stat` sees it
/// through any symbolic links.
///
/// Home Manager installs the file as a link into `/nix/store`, where every
/// file's mtime is 1: an activation points the link at another store file
/// with the same mtime, so the device and inode must count too. An editor
/// that rewrites the file in place keeps its inode and changes its times or
/// size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    size: u64,
}

impl FileIdentity {
    /// `stat` the file `path` names, following symbolic links.
    fn of(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            size: metadata.size(),
        })
    }
}

/// What the process knows of the bindings file.
#[derive(Debug, Default)]
struct LoadedFile {
    /// What the last check saw: `None` before the first check, `Some(None)`
    /// while the file cannot be found.
    seen: Option<Option<FileIdentity>>,
    /// The last good bindings; `None` while placement is off.
    bindings: Option<Vec<PlacementBinding>>,
}

/// What one [`PlacementConfig::refresh`] did. Every outcome but
/// `Unchanged` is logged once, as it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refresh {
    /// The file is as the last check saw it, or no path resolved.
    Unchanged,
    /// The file cannot be found or examined; there are no bindings.
    Absent,
    /// The file changed and its bindings replaced the last ones.
    Loaded,
    /// The file changed but cannot be used; the last good bindings, if
    /// any, stay.
    Rejected,
}

/// Process-wide window placement configuration.
#[derive(Debug)]
pub(crate) struct PlacementConfig {
    /// Parsed `SOMMELIER_ACCELERATORS`: chords the host keeps.
    host_accelerators: Vec<Accelerator>,
    /// The bindings file; `None` when no path resolved, which leaves
    /// placement off for good.
    path: Option<PathBuf>,
    file: Mutex<LoadedFile>,
}

impl PlacementConfig {
    /// Resolve the configuration from the process environment and load the
    /// bindings file.
    ///
    /// Reads `SOMMELIER_ACCELERATORS`, `XDG_CONFIG_HOME` and `HOME`, once.
    /// This never fails: every problem is logged and leaves placement off
    /// or key filtering disabled, because the proxy serves every app in the
    /// container. The rules are specified in
    /// `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`.
    pub(crate) fn from_environment() -> Self {
        let host_accelerators =
            host_accelerators(std::env::var_os(HOST_ACCELERATORS_VARIABLE).as_deref());
        let path = config_path(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        if path.is_none() {
            log::debug!(
                "Neither XDG_CONFIG_HOME nor HOME names an absolute directory, so there is no \
                 window placement configuration"
            );
        }
        Self::with_file(host_accelerators, path)
    }

    /// The configuration of `host_accelerators` and the bindings file at
    /// `path`, if any, loaded once.
    fn with_file(host_accelerators: Vec<Accelerator>, path: Option<PathBuf>) -> Self {
        let config = Self {
            host_accelerators,
            path,
            file: Mutex::default(),
        };
        config.refresh();
        config
    }

    fn file(&self) -> MutexGuard<'_, LoadedFile> {
        // Every update leaves `LoadedFile` whole, so a poisoned lock holds a
        // usable state.
        self.file.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load the bindings file again if its [`FileIdentity`] changed since
    /// the last check.
    ///
    /// Costs one `stat` when the file did not change. A file that cannot be
    /// found clears the bindings; one that cannot be used keeps the last
    /// good bindings. Either is logged once per change of the file, never
    /// per call.
    pub(crate) fn refresh(&self) -> Refresh {
        let Some(path) = &self.path else {
            return Refresh::Unchanged;
        };
        let identity = FileIdentity::of(path);
        let mut file = self.file();
        let first = file.seen.is_none();
        let seen = identity.as_ref().ok().copied();
        if file.seen.replace(seen) == Some(seen) {
            return Refresh::Unchanged;
        }
        let had_bindings = file.bindings.is_some();
        if let Err(error) = identity {
            file.bindings = None;
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "Cannot examine {}: {error}. Window placement is off.",
                    path.display()
                );
            } else if had_bindings {
                log::info!("{} was removed; window placement is off", path.display());
            } else {
                log::debug!(
                    "No window placement configuration at {}; placement is off",
                    path.display()
                );
            }
            return Refresh::Absent;
        }
        match load_shortcut_config(path, &self.host_accelerators) {
            Ok(bindings) => {
                log::info!(
                    "Loaded {} window placement binding(s) from {}{}",
                    bindings.len(),
                    path.display(),
                    if had_bindings || first {
                        ""
                    } else {
                        "; apps that connected while placement was off are placed once \
                         they reconnect"
                    }
                );
                file.bindings = Some(bindings);
                Refresh::Loaded
            }
            Err(error) => {
                log::warn!(
                    "Ignoring {}: {error}. {}",
                    path.display(),
                    if had_bindings {
                        "Keeping the last good window placement bindings."
                    } else {
                        "Window placement is off."
                    }
                );
                Refresh::Rejected
            }
        }
    }

    /// Return whether the host keeps this chord (`SOMMELIER_ACCELERATORS`).
    pub(crate) fn is_host_accelerator(&self, chord: &Accelerator) -> bool {
        self.host_accelerators.contains(chord)
    }

    /// Return the action bound to a chord, as of the last
    /// [`refresh`](Self::refresh); always `None` while placement is off.
    pub(crate) fn action_for(&self, chord: &Accelerator) -> Option<WindowLayoutAction> {
        self.file()
            .bindings
            .as_ref()?
            .iter()
            .find(|binding| binding.chord == *chord)
            .map(|binding| binding.action)
    }

    /// Return whether placement is on: the bindings file was valid when
    /// last loaded and has not gone since. Its bindings may be none.
    pub(crate) fn is_active(&self) -> bool {
        self.file().bindings.is_some()
    }
}

/// One binding as written in the file, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    chord: String,
    action: Option<String>,
    rect: Option<[f64; 4]>,
}

/// Validate the file's bindings.
///
/// # Arguments
///
/// * `raw` - the bindings in document order.
/// * `host_accelerators` - a binding for one of these chords is rejected.
///
/// # Errors
///
/// Returns the first problem found: too many bindings, an invalid or
/// duplicate chord (compared after parsing), a host-accelerator conflict, an
/// unknown action, a binding with both or neither of `action`/`rect`, or an
/// invalid rectangle. Nothing is partially accepted.
fn validate_bindings(
    raw: Vec<RawBinding>,
    host_accelerators: &[Accelerator],
) -> Result<Vec<PlacementBinding>, BindingsError> {
    if raw.len() > MAX_BINDINGS {
        return Err(BindingsError::TooManyBindings { count: raw.len() });
    }
    let mut bindings: Vec<PlacementBinding> = Vec::with_capacity(raw.len());
    for (index, binding) in raw.into_iter().enumerate() {
        let chord =
            parse_accelerator(&binding.chord).map_err(|source| BindingsError::InvalidChord {
                index,
                chord: binding.chord.clone(),
                source,
            })?;
        if let Some(first) = bindings.iter().position(|existing| existing.chord == chord) {
            return Err(BindingsError::DuplicateChord {
                index,
                first,
                chord: binding.chord,
            });
        }
        if host_accelerators.contains(&chord) {
            return Err(BindingsError::HostAcceleratorConflict {
                index,
                chord: binding.chord,
            });
        }
        let action = match (binding.action, binding.rect) {
            (Some(name), None) => {
                WindowLayoutAction::from_name(&name).ok_or(BindingsError::UnknownAction {
                    index,
                    action: name,
                })?
            }
            (None, Some(rect)) => {
                let [x, y, width, height] = rect;
                let rect_value = NormalizedRect::new(x, y, width, height).map_err(|source| {
                    BindingsError::InvalidRect {
                        index,
                        rect,
                        source,
                    }
                })?;
                WindowLayoutAction::Rect(rect_value)
            }
            _ => return Err(BindingsError::AmbiguousTarget { index }),
        };
        bindings.push(PlacementBinding { chord, action });
    }
    Ok(bindings)
}

/// Version probe parsed before the strict schema, so a document written for
/// a newer version reports the version instead of its first unknown field.
#[derive(Deserialize)]
struct VersionProbe {
    version: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[allow(dead_code)]
    version: i64,
    #[serde(default)]
    bindings: Vec<RawBinding>,
}

/// Parse and validate one complete configuration document.
///
/// # Arguments
///
/// * `text` - the TOML document.
/// * `host_accelerators` - the parsed `SOMMELIER_ACCELERATORS`.
///
/// # Returns
///
/// The bindings in document order. An empty `bindings` list is valid and
/// binds no chord.
///
/// # Errors
///
/// Malformed TOML, an unsupported `version`, unknown fields, or any
/// [`validate_bindings`] error.
fn parse_shortcut_config(
    text: &str,
    host_accelerators: &[Accelerator],
) -> Result<Vec<PlacementBinding>, BindingsError> {
    let VersionProbe { version } = toml::from_str(text)?;
    if version != CONFIG_VERSION {
        return Err(BindingsError::UnsupportedVersion { found: version });
    }
    let raw: RawConfig = toml::from_str(text)?;
    validate_bindings(raw.bindings, host_accelerators)
}

/// Read a configuration file with a bounded, non-blocking read.
///
/// The file is opened with `O_NONBLOCK | O_NOCTTY` and must be a regular file
/// (checked on the opened descriptor, so a FIFO or device cannot stall the
/// single-threaded proxy and the check cannot race a rename). Symbolic links
/// are followed, so a link managed by a dotfile tool works.
fn read_config_text(path: &Path) -> Result<String, BindingsError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(BindingsError::Open)?;
    let metadata = file.metadata().map_err(BindingsError::Read)?;
    if !metadata.file_type().is_file() {
        return Err(BindingsError::NotRegularFile);
    }
    // Bound the read itself instead of trusting the size reported by stat;
    // the extra byte distinguishes an at-limit file from an oversized one.
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(BindingsError::Read)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(BindingsError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| BindingsError::NotUtf8)
}

/// Read and validate a configuration file.
///
/// # Arguments
///
/// * `path` - the configuration file; it must be a regular file of at most
///   64 KiB of UTF-8 text.
/// * `host_accelerators` - the parsed `SOMMELIER_ACCELERATORS`.
///
/// # Returns
///
/// The validated bindings, see [`parse_shortcut_config`].
///
/// # Errors
///
/// Returns filesystem, size, encoding, or validation errors. The caller owns
/// the decision to keep a previous generation.
fn load_shortcut_config(
    path: &Path,
    host_accelerators: &[Accelerator],
) -> Result<Vec<PlacementBinding>, BindingsError> {
    parse_shortcut_config(&read_config_text(path)?, host_accelerators)
}

#[cfg(test)]
impl PlacementConfig {
    /// Placement off, with the given host accelerators.
    pub(crate) fn for_test(host_accelerators: Vec<Accelerator>) -> Self {
        Self::with_file(host_accelerators, None)
    }

    /// Placement on, with `bindings` (chord, named action), which may be
    /// none, and no file.
    pub(crate) fn active_for_test(bindings: &[(&str, &str)]) -> Self {
        Self::loaded_for_test(Vec::new(), bindings)
    }

    /// [`active_for_test`](Self::active_for_test) with host accelerators.
    pub(crate) fn loaded_for_test(
        host_accelerators: Vec<Accelerator>,
        bindings: &[(&str, &str)],
    ) -> Self {
        let raw = bindings
            .iter()
            .map(|&(chord, action)| RawBinding {
                chord: chord.to_string(),
                action: Some(action.to_string()),
                rect: None,
            })
            .collect();
        let bindings = validate_bindings(raw, &host_accelerators)
            .unwrap_or_else(|error| panic!("invalid test bindings {bindings:?}: {error}"));
        let config = Self::for_test(host_accelerators);
        config.file().bindings = Some(bindings);
        config
    }

    /// The configuration of the bindings file at `path`, loaded once, as
    /// `main` builds it.
    pub(crate) fn file_for_test(path: &Path) -> Self {
        Self::with_file(Vec::new(), Some(path.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accelerator::ALT_MASK;
    use crate::test_support::TempDir;
    use std::os::unix::ffi::OsStrExt;
    use xkbcommon::xkb;

    const VALID: &str = r#"
version = 1

[[bindings]]
chord = "<Alt>q"
rect = [0.0, 0.0, 0.5, 0.5]

[[bindings]]
chord = "<Alt>s"
action = "fullscreen"
"#;

    fn parse(text: &str) -> Result<Vec<PlacementBinding>, BindingsError> {
        parse_shortcut_config(text, &[])
    }

    fn one_binding(fields: &str) -> String {
        format!("version = 1\n[[bindings]]\nchord = \"<Alt>q\"\n{fields}\n")
    }

    fn alt(symbol: u32) -> Accelerator {
        Accelerator {
            modifiers: ALT_MASK,
            symbol,
        }
    }

    fn alt_q(config: &PlacementConfig) -> Option<WindowLayoutAction> {
        config.action_for(&alt(xkb::keysyms::KEY_q))
    }

    /// Replace `name` in `dir` as an editor or dotfile tool does: write a
    /// new file and rename it over the old one.
    fn replace(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
        let staged = dir.write(".staged", contents.as_bytes());
        let path = dir.path().join(name);
        std::fs::rename(staged, &path).expect("rename the staged file");
        path
    }

    // ── Validation ────────────────────────────────────────────────────

    #[test]
    fn parses_rect_and_named_bindings_in_document_order() {
        let bindings = parse(VALID).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].chord, alt(xkb::keysyms::KEY_q));
        assert_eq!(
            bindings[0].action,
            WindowLayoutAction::Rect(NormalizedRect::new(0.0, 0.0, 0.5, 0.5).unwrap())
        );
        assert_eq!(bindings[1].action, WindowLayoutAction::Fullscreen);
    }

    #[test]
    fn rejects_host_accelerator_conflicts() {
        let host = vec![parse_accelerator("<Alt>q").unwrap()];
        assert!(matches!(
            parse_shortcut_config(&one_binding("action = \"left\""), &host),
            Err(BindingsError::HostAcceleratorConflict { index: 0, .. })
        ));
    }

    #[test]
    fn accepts_integer_rect_components_and_empty_binding_list() {
        let bindings = parse(&one_binding("rect = [0, 0, 1, 1]")).unwrap();
        assert_eq!(
            bindings[0].action,
            WindowLayoutAction::Rect(NormalizedRect::new(0.0, 0.0, 1.0, 1.0).unwrap())
        );
        assert!(parse("version = 1\n").unwrap().is_empty());
        assert!(parse("version = 1\nbindings = []\n").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_toml_and_unknown_fields() {
        for text in [
            "version = 1\n[[bindings]\n".to_string(),
            "version = 1\nextra = true\n".to_string(),
            one_binding("action = \"left\"\nrepeat = true"),
            "[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n".to_string(),
            one_binding("rect = [0.0, 0.0, 0.5]"),
        ] {
            assert!(
                matches!(parse(&text), Err(BindingsError::Toml(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_version_before_schema_fields() {
        assert!(matches!(
            parse("version = 2\nfuture_field = true\n"),
            Err(BindingsError::UnsupportedVersion { found: 2 })
        ));
    }

    #[test]
    fn rejects_duplicate_chords_by_parsed_accelerator() {
        let text = format!(
            "{}\n[[bindings]]\nchord = \"<Meta>Q\"\naction = \"left\"\n",
            one_binding("action = \"right\"")
        );
        match parse(&text) {
            Err(BindingsError::DuplicateChord { index, first, .. }) => {
                assert_eq!((index, first), (1, 0));
            }
            other => panic!("expected duplicate chord, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_chords_actions_and_targets() {
        assert!(matches!(
            parse("version = 1\n[[bindings]]\nchord = \"<Hyper>q\"\naction = \"left\"\n"),
            Err(BindingsError::InvalidChord { index: 0, .. })
        ));
        assert!(matches!(
            parse(&one_binding("action = \"tile\"")),
            Err(BindingsError::UnknownAction { index: 0, .. })
        ));
        assert!(matches!(
            parse(&one_binding("")),
            Err(BindingsError::AmbiguousTarget { index: 0 })
        ));
        assert!(matches!(
            parse(&one_binding(
                "action = \"left\"\nrect = [0.0, 0.0, 0.5, 1.0]"
            )),
            Err(BindingsError::AmbiguousTarget { index: 0 })
        ));
    }

    #[test]
    fn rejects_invalid_rects() {
        for (rect, expected) in [
            ("[0.0, 0.0, 0.0, 0.5]", RectError::SizeOutOfRange),
            ("[0.0, 0.0, 0.5, 0.0]", RectError::SizeOutOfRange),
            ("[-0.5, 0.0, 0.5, 0.5]", RectError::OriginOutOfRange),
            ("[0.0, 1.0, 0.5, 0.5]", RectError::OriginOutOfRange),
            ("[0.0, 0.0, 1.5, 0.5]", RectError::SizeOutOfRange),
            ("[0.6, 0.0, 0.5, 0.5]", RectError::ExceedsWorkArea),
            ("[0.0, 0.6, 0.5, 0.5]", RectError::ExceedsWorkArea),
            ("[nan, 0.0, 0.5, 0.5]", RectError::NonFinite),
            ("[0.0, 0.0, inf, 0.5]", RectError::NonFinite),
        ] {
            match parse(&one_binding(&format!("rect = {rect}"))) {
                Err(BindingsError::InvalidRect { index, source, .. }) => {
                    assert_eq!((index, source), (0, expected), "{rect}");
                }
                other => panic!("{rect}: expected invalid rect, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_too_many_bindings() {
        let mut text = String::from("version = 1\n");
        for _ in 0..=MAX_BINDINGS {
            text.push_str("[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n");
        }
        assert!(matches!(
            parse(&text),
            Err(BindingsError::TooManyBindings { count }) if count == MAX_BINDINGS + 1
        ));
    }

    #[test]
    fn load_rejects_oversized_non_utf8_missing_and_non_regular_files() {
        let dir = TempDir::new("shortcut-load");
        let oversized = dir.write("big.toml", &vec![b'#'; MAX_CONFIG_BYTES as usize + 1]);
        assert!(matches!(
            load_shortcut_config(&oversized, &[]),
            Err(BindingsError::TooLarge)
        ));
        let mut at_limit = b"version = 1\n".to_vec();
        at_limit.resize(MAX_CONFIG_BYTES as usize, b' ');
        let at_limit = dir.write("limit.toml", &at_limit);
        assert!(load_shortcut_config(&at_limit, &[]).unwrap().is_empty());
        let binary = dir.write("binary.toml", b"version = 1\n# \xff\n");
        assert!(matches!(
            load_shortcut_config(&binary, &[]),
            Err(BindingsError::NotUtf8)
        ));
        assert!(matches!(
            load_shortcut_config(&dir.path().join("missing.toml"), &[]),
            Err(BindingsError::Open(_))
        ));
        assert!(matches!(
            load_shortcut_config(dir.path(), &[]),
            Err(BindingsError::NotRegularFile)
        ));
        let fifo = dir.path().join("fifo.toml");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("create test FIFO");
        assert!(
            matches!(
                load_shortcut_config(&fifo, &[]),
                Err(BindingsError::NotRegularFile)
            ),
            "a FIFO must be rejected without blocking on a writer"
        );
    }

    // ── Location ──────────────────────────────────────────────────────

    #[test]
    fn the_file_is_under_xdg_config_home_or_home() {
        let os = |text: &'static str| Some(OsStr::from_bytes(text.as_bytes()));
        let file = |dir: &str| Some(Path::new(dir).join("sommelier-rs/window-placement.toml"));
        for (xdg_config_home, home, path) in [
            (os("/xdg"), os("/home/u"), file("/xdg")),
            (None, os("/home/u"), file("/home/u/.config")),
            // Empty or relative counts as unset.
            (os(""), os("/home/u"), file("/home/u/.config")),
            (os("xdg"), os("/home/u"), file("/home/u/.config")),
            (os("/xdg"), None, file("/xdg")),
            (None, None, None),
            (None, os(""), None),
            (os("xdg"), os("home"), None),
        ] {
            assert_eq!(
                config_path(xdg_config_home, home),
                path,
                "{xdg_config_home:?} {home:?}"
            );
        }
    }

    #[test]
    fn host_accelerators_are_unset_valid_or_ignored() {
        let os = |text: &'static str| Some(OsStr::from_bytes(text.as_bytes()));
        assert_eq!(host_accelerators(None), Vec::new());
        assert_eq!(host_accelerators(os("  ")), Vec::new());
        assert_eq!(
            host_accelerators(os("<Alt>a")),
            vec![parse_accelerator("<Alt>a").unwrap()]
        );
        assert_eq!(host_accelerators(os("<Hyper>a")), Vec::new());
        assert_eq!(
            host_accelerators(Some(OsStr::from_bytes(b"\xff"))),
            Vec::new()
        );
    }

    // ── Loading and reloading ─────────────────────────────────────────

    /// No file, or no path at all, is placement off, and costs no warning.
    #[test]
    fn a_missing_file_leaves_placement_off() {
        let dir = TempDir::new("placement-missing");
        let config = PlacementConfig::file_for_test(&dir.path().join("window-placement.toml"));
        assert!(!config.is_active());
        assert_eq!(alt_q(&config), None);
        assert_eq!(config.refresh(), Refresh::Unchanged);

        let config = PlacementConfig::with_file(Vec::new(), None);
        assert!(!config.is_active());
        assert_eq!(config.refresh(), Refresh::Unchanged);
    }

    /// A file that cannot be used at startup leaves placement off, and its
    /// warning is logged once: a later check of the same file is
    /// `Unchanged`.
    #[test]
    fn an_invalid_file_at_startup_leaves_placement_off() {
        let dir = TempDir::new("placement-invalid");
        for invalid in [
            "not toml",
            "version = 1\n[[bindings]]\nchord = \"<Alt>q\"\n",
            "version = 2\n",
        ] {
            let path = replace(&dir, "window-placement.toml", invalid);
            let config = PlacementConfig::file_for_test(&path);
            assert!(!config.is_active(), "{invalid}");
            assert_eq!(alt_q(&config), None);
            assert_eq!(config.refresh(), Refresh::Unchanged);
        }
        let path = replace(
            &dir,
            "window-placement.toml",
            &one_binding("action = \"left\""),
        );
        let host = vec![parse_accelerator("<Alt>q").unwrap()];
        let config = PlacementConfig::with_file(host, Some(path));
        assert!(!config.is_active(), "a host accelerator cannot be bound");
    }

    #[test]
    fn a_valid_file_turns_placement_on() {
        let dir = TempDir::new("placement-valid");
        let path = replace(&dir, "window-placement.toml", VALID);
        let config = PlacementConfig::file_for_test(&path);
        assert!(config.is_active());
        assert_eq!(
            config.action_for(&alt(xkb::keysyms::KEY_s)),
            Some(WindowLayoutAction::Fullscreen)
        );
        assert_eq!(config.action_for(&alt(xkb::keysyms::KEY_w)), None);

        let path = replace(&dir, "empty.toml", "version = 1\n");
        let config = PlacementConfig::file_for_test(&path);
        assert!(config.is_active(), "a valid file with no bindings");
        assert_eq!(alt_q(&config), None);
    }

    /// A changed file replaces the bindings on the next check; a bad edit
    /// keeps the last good ones and is reported once, however often the
    /// file is checked; removing the file clears them; and a file that
    /// appears later is loaded.
    #[test]
    fn refresh_follows_the_file() {
        let dir = TempDir::new("placement-refresh");
        let bind_alt_q = |action: &str| {
            replace(
                &dir,
                "window-placement.toml",
                &one_binding(&format!("action = \"{action}\"")),
            )
        };
        let path = bind_alt_q("left");
        let config = PlacementConfig::file_for_test(&path);
        assert_eq!(alt_q(&config), Some(WindowLayoutAction::Left));
        assert_eq!(config.refresh(), Refresh::Unchanged);

        bind_alt_q("right");
        assert_eq!(
            alt_q(&config),
            Some(WindowLayoutAction::Left),
            "only a check adopts a change"
        );
        assert_eq!(config.refresh(), Refresh::Loaded);
        assert_eq!(alt_q(&config), Some(WindowLayoutAction::Right));

        for invalid in ["not toml", "version = 1\nbindings = 3\n"] {
            replace(&dir, "window-placement.toml", invalid);
            assert_eq!(config.refresh(), Refresh::Rejected, "{invalid}");
            assert_eq!(config.refresh(), Refresh::Unchanged, "warned once");
            assert_eq!(
                alt_q(&config),
                Some(WindowLayoutAction::Right),
                "the last good bindings stay"
            );
        }

        std::fs::remove_file(&path).unwrap();
        assert_eq!(config.refresh(), Refresh::Absent);
        assert!(!config.is_active());
        assert_eq!(alt_q(&config), None);
        assert_eq!(config.refresh(), Refresh::Unchanged);

        bind_alt_q("top");
        assert_eq!(config.refresh(), Refresh::Loaded);
        assert_eq!(alt_q(&config), Some(WindowLayoutAction::Top));
    }

    /// A file that cannot be examined for another reason than that it is
    /// missing, here because a directory on its path became a file, clears
    /// the bindings like a removed one and is warned about once.
    #[test]
    fn a_file_that_cannot_be_examined_clears_the_bindings() {
        let dir = TempDir::new("placement-unexaminable");
        let parent = dir.path().join("sommelier-rs");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("window-placement.toml");
        std::fs::write(&path, one_binding("action = \"left\"")).unwrap();
        let config = PlacementConfig::file_for_test(&path);
        assert_eq!(alt_q(&config), Some(WindowLayoutAction::Left));

        std::fs::remove_dir_all(&parent).unwrap();
        std::fs::write(&parent, "").unwrap();
        assert_eq!(
            FileIdentity::of(&path).map_err(|error| error.kind()),
            Err(io::ErrorKind::NotADirectory),
            "not a missing file"
        );
        assert_eq!(config.refresh(), Refresh::Absent);
        assert!(!config.is_active());
        assert_eq!(alt_q(&config), None);
        assert_eq!(config.refresh(), Refresh::Unchanged, "warned once");
    }

    /// An edit in place, which keeps the inode, is seen too.
    #[test]
    fn refresh_sees_an_edit_in_place() {
        let dir = TempDir::new("placement-in-place");
        let path = dir.write(
            "window-placement.toml",
            one_binding("action = \"left\"").as_bytes(),
        );
        let config = PlacementConfig::file_for_test(&path);
        dir.write(
            "window-placement.toml",
            one_binding("action = \"bottom\"").as_bytes(),
        );
        assert_eq!(config.refresh(), Refresh::Loaded);
        assert_eq!(alt_q(&config), Some(WindowLayoutAction::Bottom));
    }

    /// Home Manager links the file into `/nix/store`, where every file's
    /// mtime is 1, and an activation points the link at another store file:
    /// same mtime, here the same size too, but another inode.
    #[test]
    fn refresh_follows_a_link_to_files_of_the_same_mtime() {
        let dir = TempDir::new("placement-link");
        let store_file = |name: &str, x: &str| {
            let path = dir.write(
                name,
                one_binding(&format!("rect = [{x}, 0.0, 0.5, 1.0]")).as_bytes(),
            );
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| {
                    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                })
                .expect("set the store file's mtime");
            path
        };
        let left = store_file("left.toml", "0.0");
        let right = store_file("right.toml", "0.5");
        let link = dir.path().join("window-placement.toml");
        std::os::unix::fs::symlink(&left, &link).unwrap();
        let config = PlacementConfig::file_for_test(&link);
        let half = |x| {
            Some(WindowLayoutAction::Rect(
                NormalizedRect::new(x, 0.0, 0.5, 1.0).unwrap(),
            ))
        };
        assert_eq!(alt_q(&config), half(0.0));

        let staged = dir.path().join(".staged-link");
        std::os::unix::fs::symlink(&right, &staged).unwrap();
        std::fs::rename(&staged, &link).unwrap();
        assert_eq!(config.refresh(), Refresh::Loaded);
        assert_eq!(alt_q(&config), half(0.5));
    }
}
