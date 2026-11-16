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

//! The optional window placement shortcut configuration file.
//!
//! `--window-shortcuts-config PATH` names a versioned TOML document whose
//! bindings replace `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` as the single
//! source of compositor-owned placement chords:
//!
//! ```toml
//! version = 1
//!
//! [[bindings]]
//! chord = "<Alt>q"
//! rect = [0.0, 0.0, 0.5, 0.5]   # x, y, width, height of the work area
//!
//! [[bindings]]
//! chord = "<Alt>s"
//! action = "fullscreen"          # or one of the named presets
//! ```
//!
//! Exports:
//!
//! * [`parse_shortcut_config`] / [`load_shortcut_config`] - strict, fail-closed
//!   validation of one complete document (schema, chords, duplicates,
//!   `SOMMELIER_ACCELERATORS` conflicts, rectangles, size bound).
//! * [`ShortcutConfigSource`] - the process-wide owner of the file path and
//!   the last known-good bindings. [`ShortcutConfigSource::reload`] validates
//!   a new document completely before publishing it, so an invalid reload
//!   keeps the previous bindings.
//! * [`ShortcutSubscription`] - a per-connection view that adopts a newly
//!   published generation the next time a key press is matched.
//! * [`startup_source_from_environment`] - the startup gate: the file is read
//!   only when the option is given and `SOMMELIER_WINDOW_BOUNDS_AS_ARC` is
//!   requested, and combining it with the environment bindings is an error.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::watch;

use crate::accelerator::{parse_accelerator, Accelerator, ParseError, WindowPlacementShortcut};
use crate::window_layout::{NormalizedRect, RectError, WindowLayoutAction};

/// The only schema version this build understands.
pub(crate) const CONFIG_VERSION: i64 = 1;
/// Upper bound on the file size read from disk. Real configurations are a few
/// kilobytes; the bound keeps a mistaken path from stalling the proxy.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// Upper bound on the number of bindings in one document.
const MAX_BINDINGS: usize = 256;

/// One immutable, validated generation of bindings.
pub(crate) type ShortcutBindings = Arc<[WindowPlacementShortcut]>;

/// Why a shortcut configuration document was rejected.
#[derive(Debug, Error)]
pub(crate) enum ShortcutConfigError {
    #[error("cannot open {}: {source}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a regular file", path.display())]
    NotRegularFile { path: PathBuf },
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} exceeds the {limit}-byte size limit", path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("{} is not valid UTF-8", path.display())]
    NotUtf8 { path: PathBuf },
    #[error("malformed TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("unsupported version {found}; this build understands version {CONFIG_VERSION}")]
    UnsupportedVersion { found: i64 },
    #[error("{count} bindings exceed the limit of {limit}")]
    TooManyBindings { count: usize, limit: usize },
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
    #[error("bindings[{index}]: chord {chord:?} conflicts with SOMMELIER_ACCELERATORS")]
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

/// Why the startup shortcut configuration could not be established.
#[derive(Debug, Error)]
pub(crate) enum ShortcutStartupError {
    #[error(
        "--window-shortcuts-config and SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS are mutually \
         exclusive; configure placement shortcuts in one place"
    )]
    ConflictingSources,
    #[error("cannot validate --window-shortcuts-config against SOMMELIER_ACCELERATORS: {0}")]
    InvalidHostAccelerators(String),
    #[error("cannot resolve --window-shortcuts-config {}: {source}", path.display())]
    Path {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid --window-shortcuts-config: {0}")]
    Load(#[from] ShortcutConfigError),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    chord: String,
    action: Option<String>,
    rect: Option<[f64; 4]>,
}

/// Parse and validate one complete configuration document.
///
/// # Arguments
///
/// * `text` - the TOML document.
/// * `host_accelerators` - the parsed `SOMMELIER_ACCELERATORS`; a binding
///   for one of these chords is rejected.
///
/// # Returns
///
/// The bindings in document order. An empty `bindings` list is valid and
/// disables every placement shortcut.
///
/// # Errors
///
/// Returns the first [`ShortcutConfigError`] found: malformed TOML, an
/// unsupported `version`, unknown fields, too many bindings, an invalid or
/// duplicate chord, a host-accelerator conflict, an unknown action, a binding
/// with both or neither of `action`/`rect`, or an invalid rectangle. Nothing
/// is partially accepted.
pub(crate) fn parse_shortcut_config(
    text: &str,
    host_accelerators: &[Accelerator],
) -> Result<Vec<WindowPlacementShortcut>, ShortcutConfigError> {
    let VersionProbe { version } = toml::from_str(text)?;
    if version != CONFIG_VERSION {
        return Err(ShortcutConfigError::UnsupportedVersion { found: version });
    }
    let raw: RawConfig = toml::from_str(text)?;
    if raw.bindings.len() > MAX_BINDINGS {
        return Err(ShortcutConfigError::TooManyBindings {
            count: raw.bindings.len(),
            limit: MAX_BINDINGS,
        });
    }

    let mut bindings: Vec<WindowPlacementShortcut> = Vec::with_capacity(raw.bindings.len());
    for (index, binding) in raw.bindings.into_iter().enumerate() {
        let accelerator = parse_accelerator(&binding.chord).map_err(|source| {
            ShortcutConfigError::InvalidChord {
                index,
                chord: binding.chord.clone(),
                source,
            }
        })?;
        if let Some(first) = bindings
            .iter()
            .position(|existing| existing.accelerator == accelerator)
        {
            return Err(ShortcutConfigError::DuplicateChord {
                index,
                first,
                chord: binding.chord,
            });
        }
        if host_accelerators.contains(&accelerator) {
            return Err(ShortcutConfigError::HostAcceleratorConflict {
                index,
                chord: binding.chord,
            });
        }
        let action = match (binding.action, binding.rect) {
            (Some(name), None) => {
                WindowLayoutAction::from_name(&name).ok_or(ShortcutConfigError::UnknownAction {
                    index,
                    action: name,
                })?
            }
            (None, Some(rect)) => {
                let [x, y, width, height] = rect;
                let rect_value = NormalizedRect::new(x, y, width, height).map_err(|source| {
                    ShortcutConfigError::InvalidRect {
                        index,
                        rect,
                        source,
                    }
                })?;
                WindowLayoutAction::Rect(rect_value)
            }
            _ => return Err(ShortcutConfigError::AmbiguousTarget { index }),
        };
        bindings.push(WindowPlacementShortcut {
            accelerator,
            action,
        });
    }
    Ok(bindings)
}

/// Read a configuration file with a bounded, non-blocking read.
///
/// The file is opened with `O_NONBLOCK | O_NOCTTY` and must be a regular file
/// (checked on the opened descriptor, so a FIFO or device cannot stall the
/// single-threaded proxy and the check cannot race a rename). Symbolic links
/// are followed, so a link managed by a dotfile tool works.
fn read_config_text(path: &Path) -> Result<String, ShortcutConfigError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|source| ShortcutConfigError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = file
        .metadata()
        .map_err(|source| ShortcutConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(ShortcutConfigError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    // Bound the read itself instead of trusting the size reported by stat;
    // the extra byte distinguishes an at-limit file from an oversized one.
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ShortcutConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ShortcutConfigError::TooLarge {
            path: path.to_path_buf(),
            limit: MAX_CONFIG_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| ShortcutConfigError::NotUtf8 {
        path: path.to_path_buf(),
    })
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
pub(crate) fn load_shortcut_config(
    path: &Path,
    host_accelerators: &[Accelerator],
) -> Result<Vec<WindowPlacementShortcut>, ShortcutConfigError> {
    parse_shortcut_config(&read_config_text(path)?, host_accelerators)
}

/// Process-wide owner of the configuration file and its last good bindings.
///
/// The host accelerator list is captured once at startup because the process
/// environment does not change; a reload re-reads only the file.
#[derive(Debug)]
pub(crate) struct ShortcutConfigSource {
    path: PathBuf,
    host_accelerators: Vec<Accelerator>,
    current: watch::Sender<ShortcutBindings>,
}

impl ShortcutConfigSource {
    /// Load the initial configuration strictly.
    ///
    /// # Arguments
    ///
    /// * `path` - absolute path of the configuration file; reloads read the
    ///   same path.
    /// * `host_accelerators` - the parsed `SOMMELIER_ACCELERATORS`.
    ///
    /// # Errors
    ///
    /// Returns the load error; there is no previous generation to fall back
    /// on at startup.
    pub(crate) fn load(
        path: PathBuf,
        host_accelerators: Vec<Accelerator>,
    ) -> Result<Self, ShortcutConfigError> {
        let bindings = load_shortcut_config(&path, &host_accelerators)?;
        let (current, _) = watch::channel(ShortcutBindings::from(bindings));
        Ok(Self {
            path,
            host_accelerators,
            current,
        })
    }

    /// Return the configuration file path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Return the currently published bindings.
    pub(crate) fn bindings(&self) -> ShortcutBindings {
        self.current.borrow().clone()
    }

    /// Subscribe a connection to this configuration.
    pub(crate) fn subscribe(&self) -> ShortcutSubscription {
        ShortcutSubscription(self.current.subscribe())
    }

    /// Re-read the file and publish it only if it is completely valid.
    ///
    /// # Returns
    ///
    /// The number of bindings now active.
    ///
    /// # Errors
    ///
    /// Returns the load error and leaves the published bindings untouched.
    pub(crate) fn reload(&self) -> Result<usize, ShortcutConfigError> {
        let bindings = load_shortcut_config(&self.path, &self.host_accelerators)?;
        let count = bindings.len();
        self.current.send_replace(ShortcutBindings::from(bindings));
        Ok(count)
    }

    /// Handle a reload request (SIGHUP) and log its outcome.
    pub(crate) fn reload_and_log(&self) {
        match self.reload() {
            Ok(count) => log::info!(
                "Reloaded {} window placement shortcut binding(s) from {}",
                count,
                self.path.display()
            ),
            Err(error) => log::error!(
                "Keeping the last known-good window placement shortcuts; reload of {} failed: {}",
                self.path.display(),
                error
            ),
        }
    }
}

/// A connection's view of the published shortcut configuration.
#[derive(Debug)]
pub(crate) struct ShortcutSubscription(watch::Receiver<ShortcutBindings>);

impl ShortcutSubscription {
    /// Return the current bindings and mark them as seen.
    pub(crate) fn current(&mut self) -> ShortcutBindings {
        self.0.borrow_and_update().clone()
    }

    /// Return a generation published since the last call, if any.
    ///
    /// This is a non-blocking check intended for the key-press path. A source
    /// that no longer exists publishes nothing, so the last bindings stay.
    pub(crate) fn take_update(&mut self) -> Option<ShortcutBindings> {
        self.0
            .has_changed()
            .unwrap_or(false)
            .then(|| self.current())
    }
}

/// Decide whether a configuration file is in effect and load it.
///
/// # Arguments
///
/// * `config_path` - the `--window-shortcuts-config` value, if given.
/// * `arc_gate_requested` - whether `SOMMELIER_WINDOW_BOUNDS_AS_ARC` requests
///   the ARC bounds policy.
/// * `env_shortcuts_supplied` - whether `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`
///   is set to a non-blank value.
/// * `host_accelerators` - the parsed `SOMMELIER_ACCELERATORS`, or why it is
///   invalid.
///
/// # Returns
///
/// `None` when no file was given, or when the ARC gate is off (the file is
/// then not read and a warning is logged); otherwise the loaded source.
///
/// # Errors
///
/// Fails closed when both the option and the environment bindings are
/// supplied, when `SOMMELIER_ACCELERATORS` is invalid (conflicts could not be
/// checked), when the path cannot be made absolute, or when the file is
/// invalid.
pub(crate) fn startup_source(
    config_path: Option<&Path>,
    arc_gate_requested: bool,
    env_shortcuts_supplied: bool,
    host_accelerators: Result<Vec<Accelerator>, String>,
) -> Result<Option<ShortcutConfigSource>, ShortcutStartupError> {
    let Some(config_path) = config_path else {
        return Ok(None);
    };
    if env_shortcuts_supplied {
        return Err(ShortcutStartupError::ConflictingSources);
    }
    if !arc_gate_requested {
        log::warn!(
            "--window-shortcuts-config is ignored unless SOMMELIER_WINDOW_BOUNDS_AS_ARC is set; \
             {} was not read",
            config_path.display()
        );
        return Ok(None);
    }
    let host_accelerators =
        host_accelerators.map_err(ShortcutStartupError::InvalidHostAccelerators)?;
    // Resolve against the startup directory once, so every reload reads the
    // same file. Symbolic links are deliberately not resolved.
    let path = std::path::absolute(config_path).map_err(|source| ShortcutStartupError::Path {
        path: config_path.to_path_buf(),
        source,
    })?;
    Ok(Some(ShortcutConfigSource::load(path, host_accelerators)?))
}

/// Return whether an environment binding list counts as supplied.
///
/// A blank value means "no bindings", matching how `Context` parses it; a
/// value that is not UTF-8 is still a (malformed) attempt to configure.
fn env_shortcuts_supplied(value: Option<OsString>) -> bool {
    value.is_some_and(|value| value.to_str().is_none_or(|text| !text.trim().is_empty()))
}

/// Resolve the startup configuration source from the process environment.
///
/// See [`startup_source`] for the rules; this wrapper only reads
/// `SOMMELIER_WINDOW_BOUNDS_AS_ARC`, `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`,
/// and `SOMMELIER_ACCELERATORS`, and logs the loaded binding count.
///
/// # Errors
///
/// Returns the [`ShortcutStartupError`] from [`startup_source`].
pub(crate) fn startup_source_from_environment(
    config_path: Option<&Path>,
) -> Result<Option<ShortcutConfigSource>, ShortcutStartupError> {
    let source = startup_source(
        config_path,
        crate::state::WindowPlacementState::arc_bounds_requested(),
        env_shortcuts_supplied(std::env::var_os("SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS")),
        crate::accelerator::host_accelerators_from_environment(),
    )?;
    if let Some(source) = &source {
        log::info!(
            "Loaded {} window placement shortcut binding(s) from {}; send SIGHUP to reload",
            source.bindings().len(),
            source.path().display()
        );
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accelerator::ALT_MASK;
    use crate::test_support::TempDir;
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

    fn parse(text: &str) -> Result<Vec<WindowPlacementShortcut>, ShortcutConfigError> {
        parse_shortcut_config(text, &[])
    }

    fn one_binding(fields: &str) -> String {
        format!("version = 1\n[[bindings]]\nchord = \"<Alt>q\"\n{fields}\n")
    }

    #[test]
    fn parses_rect_and_named_bindings_in_document_order() {
        let bindings = parse(VALID).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(
            bindings[0].accelerator,
            Accelerator {
                modifiers: ALT_MASK,
                symbol: xkb::keysyms::KEY_q,
            }
        );
        assert_eq!(
            bindings[0].action,
            WindowLayoutAction::Rect(NormalizedRect::new(0.0, 0.0, 0.5, 0.5).unwrap())
        );
        assert_eq!(bindings[1].action, WindowLayoutAction::Fullscreen);
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
        assert!(matches!(
            parse("version = 1\n[[bindings]\n"),
            Err(ShortcutConfigError::Toml(_))
        ));
        assert!(matches!(
            parse("version = 1\nextra = true\n"),
            Err(ShortcutConfigError::Toml(_))
        ));
        assert!(matches!(
            parse(&one_binding("action = \"left\"\nrepeat = true")),
            Err(ShortcutConfigError::Toml(_))
        ));
        assert!(matches!(
            parse("[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n"),
            Err(ShortcutConfigError::Toml(_))
        ));
        assert!(matches!(
            parse(&one_binding("rect = [0.0, 0.0, 0.5]")),
            Err(ShortcutConfigError::Toml(_))
        ));
    }

    #[test]
    fn rejects_unsupported_version_before_schema_fields() {
        assert!(matches!(
            parse("version = 2\nfuture_field = true\n"),
            Err(ShortcutConfigError::UnsupportedVersion { found: 2 })
        ));
    }

    #[test]
    fn rejects_duplicate_chords_by_parsed_accelerator() {
        let text = format!(
            "{}\n[[bindings]]\nchord = \"<Meta>Q\"\naction = \"left\"\n",
            one_binding("action = \"right\"")
        );
        match parse(&text) {
            Err(ShortcutConfigError::DuplicateChord { index, first, .. }) => {
                assert_eq!((index, first), (1, 0));
            }
            other => panic!("expected duplicate chord, got {other:?}"),
        }
    }

    #[test]
    fn rejects_host_accelerator_conflicts() {
        let host = vec![parse_accelerator("<Alt>q").unwrap()];
        assert!(matches!(
            parse_shortcut_config(&one_binding("action = \"left\""), &host),
            Err(ShortcutConfigError::HostAcceleratorConflict { index: 0, .. })
        ));
    }

    #[test]
    fn rejects_bad_chords_actions_and_targets() {
        assert!(matches!(
            parse("version = 1\n[[bindings]]\nchord = \"<Hyper>q\"\naction = \"left\"\n"),
            Err(ShortcutConfigError::InvalidChord { index: 0, .. })
        ));
        assert!(matches!(
            parse(&one_binding("action = \"tile\"")),
            Err(ShortcutConfigError::UnknownAction { index: 0, .. })
        ));
        assert!(matches!(
            parse(&one_binding("")),
            Err(ShortcutConfigError::AmbiguousTarget { index: 0 })
        ));
        assert!(matches!(
            parse(&one_binding(
                "action = \"left\"\nrect = [0.0, 0.0, 0.5, 1.0]"
            )),
            Err(ShortcutConfigError::AmbiguousTarget { index: 0 })
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
                Err(ShortcutConfigError::InvalidRect { index, source, .. }) => {
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
            Err(ShortcutConfigError::TooManyBindings { count, .. }) if count == MAX_BINDINGS + 1
        ));
    }

    #[test]
    fn load_rejects_oversized_non_utf8_missing_and_non_regular_files() {
        let dir = TempDir::new("shortcut-load");
        let oversized = dir.write("big.toml", &vec![b'#'; MAX_CONFIG_BYTES as usize + 1]);
        assert!(matches!(
            load_shortcut_config(&oversized, &[]),
            Err(ShortcutConfigError::TooLarge { .. })
        ));
        let mut at_limit = b"version = 1\n".to_vec();
        at_limit.resize(MAX_CONFIG_BYTES as usize, b' ');
        let at_limit = dir.write("limit.toml", &at_limit);
        assert!(load_shortcut_config(&at_limit, &[]).unwrap().is_empty());
        let binary = dir.write("binary.toml", b"version = 1\n# \xff\n");
        assert!(matches!(
            load_shortcut_config(&binary, &[]),
            Err(ShortcutConfigError::NotUtf8 { .. })
        ));
        assert!(matches!(
            load_shortcut_config(&dir.path().join("missing.toml"), &[]),
            Err(ShortcutConfigError::Open { .. })
        ));
        assert!(matches!(
            load_shortcut_config(dir.path(), &[]),
            Err(ShortcutConfigError::NotRegularFile { .. })
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
                Err(ShortcutConfigError::NotRegularFile { .. })
            ),
            "a FIFO must be rejected without blocking on a writer"
        );
    }

    #[test]
    fn load_follows_symbolic_links() {
        let dir = TempDir::new("shortcut-symlink");
        let target = dir.write("target.toml", VALID.as_bytes());
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).expect("create test symlink");
        assert_eq!(load_shortcut_config(&link, &[]).unwrap().len(), 2);
    }

    #[test]
    fn reload_swaps_valid_generation_and_keeps_last_good_on_error() {
        let dir = TempDir::new("shortcut-reload");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());
        let source = ShortcutConfigSource::load(path.clone(), Vec::new()).unwrap();
        let mut subscription = source.subscribe();
        assert_eq!(subscription.current().len(), 2);
        assert!(subscription.take_update().is_none());

        dir.write(
            "shortcuts.toml",
            one_binding("action = \"left\"").as_bytes(),
        );
        assert_eq!(source.reload().unwrap(), 1);
        let updated = subscription.take_update().expect("valid reload publishes");
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].action, WindowLayoutAction::Left);
        assert!(subscription.take_update().is_none());

        let last_good = source.bindings();
        for invalid in [
            one_binding("rect = [0.8, 0.0, 0.5, 0.5]"),
            "version = 1\n[[bindings]]\nchord = \"<Alt>q\"\n".to_string(),
            "not toml".to_string(),
        ] {
            dir.write("shortcuts.toml", invalid.as_bytes());
            assert!(source.reload().is_err());
            source.reload_and_log();
            assert!(Arc::ptr_eq(&last_good, &source.bindings()));
            assert!(
                subscription.take_update().is_none(),
                "an invalid reload must not publish a generation"
            );
        }

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            source.reload(),
            Err(ShortcutConfigError::Open { .. })
        ));
        assert!(Arc::ptr_eq(&last_good, &source.bindings()));

        dir.write("shortcuts.toml", b"version = 1\n");
        assert_eq!(source.reload().unwrap(), 0);
        assert!(subscription.take_update().unwrap().is_empty());
    }

    #[test]
    fn reload_keeps_rejecting_startup_host_accelerators() {
        let dir = TempDir::new("shortcut-reload-host");
        let path = dir.write("shortcuts.toml", b"version = 1\n");
        let host = vec![parse_accelerator("<Alt>q").unwrap()];
        let source = ShortcutConfigSource::load(path, host).unwrap();
        dir.write(
            "shortcuts.toml",
            one_binding("action = \"left\"").as_bytes(),
        );
        assert!(matches!(
            source.reload(),
            Err(ShortcutConfigError::HostAcceleratorConflict { .. })
        ));
        assert!(source.bindings().is_empty());
    }

    #[test]
    fn subscription_keeps_bindings_after_source_is_dropped() {
        let dir = TempDir::new("shortcut-dropped");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());
        let source = ShortcutConfigSource::load(path, Vec::new()).unwrap();
        let mut subscription = source.subscribe();
        drop(source);
        assert!(subscription.take_update().is_none());
        assert_eq!(subscription.current().len(), 2);
    }

    #[test]
    fn startup_requires_option_and_arc_gate_and_a_single_source() {
        let dir = TempDir::new("shortcut-startup");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());

        assert!(startup_source(None, true, true, Err("bad".into()))
            .unwrap()
            .is_none());
        assert!(matches!(
            startup_source(Some(path.as_path()), true, true, Ok(Vec::new())),
            Err(ShortcutStartupError::ConflictingSources)
        ));
        assert!(matches!(
            startup_source(Some(path.as_path()), false, true, Ok(Vec::new())),
            Err(ShortcutStartupError::ConflictingSources)
        ));
        // Gate off: the file is not read, so even a missing path is accepted.
        assert!(startup_source(
            Some(dir.path().join("missing").as_path()),
            false,
            false,
            Ok(Vec::new())
        )
        .unwrap()
        .is_none());
        assert!(matches!(
            startup_source(Some(path.as_path()), true, false, Err("bad".into())),
            Err(ShortcutStartupError::InvalidHostAccelerators(_))
        ));
        assert!(matches!(
            startup_source(
                Some(dir.path().join("missing").as_path()),
                true,
                false,
                Ok(Vec::new())
            ),
            Err(ShortcutStartupError::Load(ShortcutConfigError::Open { .. }))
        ));
        let host = vec![parse_accelerator("<Alt>s").unwrap()];
        assert!(matches!(
            startup_source(Some(path.as_path()), true, false, Ok(host)),
            Err(ShortcutStartupError::Load(
                ShortcutConfigError::HostAcceleratorConflict { index: 1, .. }
            ))
        ));
        let source = startup_source(Some(path.as_path()), true, false, Ok(Vec::new()))
            .unwrap()
            .expect("gated option loads the file");
        assert_eq!(source.bindings().len(), 2);
        assert_eq!(source.path(), path);
    }

    #[test]
    fn startup_resolves_relative_paths_against_the_startup_directory() {
        let relative = Path::new("sommelier-shortcuts-that-do-not-exist.toml");
        match startup_source(Some(relative), true, false, Ok(Vec::new())) {
            Err(ShortcutStartupError::Load(ShortcutConfigError::Open { path, .. })) => {
                assert!(path.is_absolute());
                assert_eq!(path, std::env::current_dir().unwrap().join(relative));
            }
            other => panic!("expected absolute open failure, got {other:?}"),
        }
    }

    #[test]
    fn environment_bindings_count_as_supplied_unless_blank() {
        use std::os::unix::ffi::OsStringExt;
        assert!(!env_shortcuts_supplied(None));
        assert!(!env_shortcuts_supplied(Some(OsString::from(""))));
        assert!(!env_shortcuts_supplied(Some(OsString::from("  "))));
        assert!(env_shortcuts_supplied(Some(OsString::from("<Alt>q=left"))));
        assert!(env_shortcuts_supplied(Some(OsString::from_vec(vec![0xff]))));
    }
}
