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

//! Window placement configuration, resolved once per process.
//!
//! [`PlacementConfig`] is the single owner of every placement setting:
//!
//! * the ARC bounds policy: the `SOMMELIER_WINDOW_BOUNDS_AS_ARC` gate and the
//!   process's ARC task-ID block, which together decide whether the policy
//!   is active;
//! * the host accelerators from `SOMMELIER_ACCELERATORS`;
//! * the placement bindings, from exactly one source:
//!   `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` (`CHORD=ACTION,...`, never
//!   reloads) or `--window-shortcuts-config PATH` (a versioned TOML file,
//!   reloaded on `SIGHUP`).
//!
//! `main` resolves it once with [`PlacementConfig::from_environment`] and
//! every connection's `Context` shares it, so no other code reads these
//! variables and every warning is logged once per process. Both binding
//! sources produce the same raw entries, which pass through one validator
//! (chord syntax, duplicates after parsing, host-accelerator conflicts,
//! actions, rectangles, size bound).
//!
//! The configuration contract (sources, precedence, failure policy, reload,
//! precision) is specified once, in
//! `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`.

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use serde::Deserialize;
use thiserror::Error;

use crate::accelerator::{parse_accelerator, parse_accelerators, Accelerator, ParseError};
use crate::arc_task_ids::ArcIdAllocator;
use crate::window_layout::{NormalizedRect, RectError, WindowLayoutAction};

/// The ARC bounds policy gate.
const ARC_BOUNDS_VARIABLE: &str = "SOMMELIER_WINDOW_BOUNDS_AS_ARC";
/// Keys the host compositor handles.
const HOST_ACCELERATORS_VARIABLE: &str = "SOMMELIER_ACCELERATORS";
/// The environment binding source.
const SHORTCUTS_VARIABLE: &str = "SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS";
/// The file binding source's command-line option.
const SHORTCUTS_OPTION: &str = "--window-shortcuts-config";

/// The only schema version this build understands.
const CONFIG_VERSION: i64 = 1;
/// Upper bound on the file size read from disk. Real configurations are a few
/// kilobytes; the bound keeps a mistaken path from stalling the proxy.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// Upper bound on the number of bindings from either source.
const MAX_BINDINGS: usize = 256;

/// One validated placement binding: a chord the compositor consumes and the
/// action it performs. Only the validator constructs one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PlacementBinding {
    chord: Accelerator,
    action: WindowLayoutAction,
}

impl PlacementBinding {
    /// Return the chord, normalized like every parsed [`Accelerator`].
    #[cfg(test)]
    const fn chord(&self) -> Accelerator {
        self.chord
    }

    /// Return the placement action.
    const fn action(&self) -> WindowLayoutAction {
        self.action
    }
}

/// One immutable, validated generation of bindings.
type Bindings = Arc<[PlacementBinding]>;

/// Why a binding source was rejected. Indices count bindings from zero; in
/// the environment list, empty entries are not counted.
#[derive(Debug, Error)]
pub(crate) enum BindingsError {
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
    #[error("bindings[{index}]: {entry:?} must use CHORD=ACTION syntax")]
    MissingAction { index: usize, entry: String },
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

/// Why a requested binding source cannot be used at startup.
///
/// Never fatal: the resolver logs it once and starts with placement
/// shortcuts disabled, because the proxy serves every app in the container.
#[derive(Debug, Error)]
enum PlacementConfigError {
    #[error(
        "{SHORTCUTS_OPTION} and {SHORTCUTS_VARIABLE} are mutually exclusive; configure \
         placement shortcuts in one place"
    )]
    ConflictingSources,
    #[error("{variable} is not valid UTF-8")]
    NotUnicode { variable: &'static str },
    #[error(
        "cannot check placement bindings: invalid {HOST_ACCELERATORS_VARIABLE} {value:?}: {source}"
    )]
    InvalidHostAccelerators {
        value: String,
        #[source]
        source: ParseError,
    },
    #[error("cannot resolve {SHORTCUTS_OPTION} {}: {source}", path.display())]
    Path {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid {SHORTCUTS_OPTION}: {0}")]
    File(#[source] BindingsError),
    #[error("invalid {SHORTCUTS_VARIABLE}: {0}")]
    Environment(#[source] BindingsError),
}

/// The process environment that configures placement, captured once.
#[derive(Debug, Default)]
struct PlacementEnvironment {
    arc_bounds: Option<OsString>,
    host_accelerators: Option<OsString>,
    shortcuts: Option<OsString>,
}

impl PlacementEnvironment {
    fn read() -> Self {
        Self {
            arc_bounds: std::env::var_os(ARC_BOUNDS_VARIABLE),
            host_accelerators: std::env::var_os(HOST_ACCELERATORS_VARIABLE),
            shortcuts: std::env::var_os(SHORTCUTS_VARIABLE),
        }
    }

    /// Return whether the gate requests the ARC bounds policy.
    ///
    /// Mere presence is not enough: `VAR=0`, `VAR=false`, an exported empty
    /// value, and a non-UTF-8 value leave the policy off; the conventional
    /// explicit true spellings turn it on.
    fn arc_bounds_requested(&self) -> bool {
        self.arc_bounds
            .as_deref()
            .and_then(OsStr::to_str)
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
    }

    /// Parse the host accelerators; a blank or unset variable is an empty
    /// list.
    fn host_accelerators(&self) -> Result<Vec<Accelerator>, PlacementConfigError> {
        let Some(value) = configured_text(
            self.host_accelerators.as_deref(),
            HOST_ACCELERATORS_VARIABLE,
        )?
        else {
            return Ok(Vec::new());
        };
        parse_accelerators(value).map_err(|source| PlacementConfigError::InvalidHostAccelerators {
            value: value.to_string(),
            source,
        })
    }
}

/// The one "configured text" rule for placement variables.
///
/// # Returns
///
/// `None` when the variable is unset or blank: a blank value means "not
/// configured". Otherwise the text.
///
/// # Errors
///
/// A non-UTF-8 value is a (malformed) attempt to configure, never "unset".
fn configured_text<'a>(
    value: Option<&'a OsStr>,
    variable: &'static str,
) -> Result<Option<&'a str>, PlacementConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .ok_or(PlacementConfigError::NotUnicode { variable })?;
    Ok((!text.trim().is_empty()).then_some(text))
}

/// The binding source a deployment asked for.
enum BindingSource<'a> {
    /// The environment list, or why it cannot be read as text.
    Environment(Result<&'a str, PlacementConfigError>),
    File(&'a Path),
    /// Both sources at once, which is a configuration error.
    Both(&'a Path),
}

impl BindingSource<'_> {
    /// Name the source as the user spelled it, for diagnostics.
    fn describe(&self) -> String {
        match self {
            Self::Environment(_) => SHORTCUTS_VARIABLE.to_string(),
            Self::File(path) => format!("{SHORTCUTS_OPTION} {}", path.display()),
            Self::Both(path) => {
                format!(
                    "{SHORTCUTS_OPTION} {} and {SHORTCUTS_VARIABLE}",
                    path.display()
                )
            }
        }
    }

    /// Read and validate the source.
    ///
    /// # Returns
    ///
    /// The file that `SIGHUP` re-reads, whenever the source is a file whose
    /// path resolved (even if its content is invalid, so that a later valid
    /// reload can activate it), and the bindings.
    ///
    /// # Errors
    ///
    /// The bindings are an error when both sources are given, when the
    /// environment list is not UTF-8 or invalid, or when the file path cannot
    /// be resolved or the file cannot be read or is invalid.
    fn load(
        self,
        host_accelerators: &[Accelerator],
    ) -> (
        Option<PathBuf>,
        Result<Vec<PlacementBinding>, PlacementConfigError>,
    ) {
        match self {
            Self::Both(_) => (None, Err(PlacementConfigError::ConflictingSources)),
            Self::Environment(text) => (
                None,
                text.and_then(|text| {
                    parse_environment_bindings(text, host_accelerators)
                        .map_err(PlacementConfigError::Environment)
                }),
            ),
            // Resolve against the startup directory once, so every reload
            // reads the same file. Symbolic links are deliberately not
            // resolved.
            Self::File(path) => match std::path::absolute(path) {
                Ok(path) => {
                    let bindings = load_shortcut_config(&path, host_accelerators)
                        .map_err(PlacementConfigError::File);
                    (Some(path), bindings)
                }
                Err(source) => (
                    None,
                    Err(PlacementConfigError::Path {
                        path: path.to_path_buf(),
                        source,
                    }),
                ),
            },
        }
    }
}

/// Process-wide window placement configuration.
#[derive(Debug)]
pub(crate) struct PlacementConfig {
    /// Parsed `SOMMELIER_ACCELERATORS`: chords the host keeps.
    host_accelerators: Vec<Accelerator>,
    /// `None` while the ARC bounds policy is off. Bindings live inside it,
    /// so the policy being off and a chord being consumed cannot coexist.
    arc_bounds: Option<ArcBoundsPolicy>,
}

/// The active ARC bounds policy and its bindings.
#[derive(Debug)]
struct ArcBoundsPolicy {
    /// The process's reserved ARC task-ID block.
    arc_ids: &'static ArcIdAllocator,
    /// The current generation. A reload replaces the whole `Arc`, so every
    /// lookup sees one complete generation.
    bindings: RwLock<Bindings>,
    /// The file that `SIGHUP` re-reads; `None` for the environment source,
    /// which never reloads.
    reload_path: Option<PathBuf>,
}

impl PlacementConfig {
    /// Resolve the configuration from the process environment.
    ///
    /// Reads `SOMMELIER_WINDOW_BOUNDS_AS_ARC`, `SOMMELIER_ACCELERATORS` and
    /// `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`, reserves the process's ARC
    /// task-ID block when the gate is on, and loads the bindings.
    ///
    /// This never fails. Every problem is logged here, once, and leaves
    /// placement shortcuts disabled; the rules are specified in
    /// `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`.
    ///
    /// # Arguments
    ///
    /// * `shortcuts_file` - the `--window-shortcuts-config` value, if given.
    pub(crate) fn from_environment(shortcuts_file: Option<&Path>) -> Self {
        Self::resolve(
            &PlacementEnvironment::read(),
            shortcuts_file,
            crate::arc_task_ids::process_allocator,
        )
    }

    /// Apply the resolution rules to captured inputs.
    ///
    /// `reserve_arc_ids` is called only when the gate is on.
    fn resolve(
        environment: &PlacementEnvironment,
        shortcuts_file: Option<&Path>,
        reserve_arc_ids: impl FnOnce() -> Option<&'static ArcIdAllocator>,
    ) -> Self {
        let environment_shortcuts =
            configured_text(environment.shortcuts.as_deref(), SHORTCUTS_VARIABLE).transpose();
        let source = match (shortcuts_file, environment_shortcuts) {
            (Some(path), Some(_)) => Some(BindingSource::Both(path)),
            (Some(path), None) => Some(BindingSource::File(path)),
            (None, Some(text)) => Some(BindingSource::Environment(text)),
            (None, None) => None,
        };
        let host_accelerators = environment.host_accelerators();

        let gate = environment.arc_bounds_requested();
        let arc_ids = if gate { reserve_arc_ids() } else { None };
        let Some(arc_ids) = arc_ids else {
            if let Some(source) = &source {
                // The allocator logs why it could not reserve a block.
                let reason = if gate {
                    "no ARC task-ID block could be reserved".to_string()
                } else {
                    format!("{ARC_BOUNDS_VARIABLE} is not set to a true value")
                };
                log::warn!(
                    "Window placement shortcuts from {} are ignored and not read: the ARC \
                     bounds policy is off because {reason}",
                    source.describe()
                );
            }
            return Self {
                host_accelerators: host_accelerators_or_unfiltered(host_accelerators),
                arc_bounds: None,
            };
        };

        let Some(source) = source else {
            return Self::with_arc_bounds(
                host_accelerators_or_unfiltered(host_accelerators),
                arc_ids,
                Vec::new(),
                None,
            );
        };
        let host_accelerators = match host_accelerators {
            Ok(host_accelerators) => host_accelerators,
            Err(error) => {
                // No binding can be checked against the host list, now or on
                // a reload, so the source is not read at all.
                log::error!(
                    "{error}. Window placement shortcuts and accelerator filtering are disabled."
                );
                return Self::with_arc_bounds(Vec::new(), arc_ids, Vec::new(), None);
            }
        };
        let (reload_path, bindings) = source.load(&host_accelerators);
        match bindings {
            Ok(bindings) => {
                let config =
                    Self::with_arc_bounds(host_accelerators, arc_ids, bindings, reload_path);
                config.log_loaded("Loaded");
                config
            }
            Err(error) => {
                let until = if reload_path.is_some() {
                    " until a valid SIGHUP reload"
                } else {
                    ""
                };
                log::error!("{error}. Window placement shortcuts are disabled{until}.");
                Self::with_arc_bounds(host_accelerators, arc_ids, Vec::new(), reload_path)
            }
        }
    }

    fn with_arc_bounds(
        host_accelerators: Vec<Accelerator>,
        arc_ids: &'static ArcIdAllocator,
        bindings: Vec<PlacementBinding>,
        reload_path: Option<PathBuf>,
    ) -> Self {
        Self {
            host_accelerators,
            arc_bounds: Some(ArcBoundsPolicy {
                arc_ids,
                bindings: RwLock::new(Bindings::from(bindings)),
                reload_path,
            }),
        }
    }

    /// Return whether the host keeps this chord (`SOMMELIER_ACCELERATORS`).
    pub(crate) fn is_host_accelerator(&self, chord: &Accelerator) -> bool {
        self.host_accelerators.contains(chord)
    }

    /// Return the action bound to a chord in the current generation.
    ///
    /// Always `None` while the ARC bounds policy is off. A generation
    /// published by a reload is visible to the next lookup.
    pub(crate) fn action_for(&self, chord: &Accelerator) -> Option<WindowLayoutAction> {
        self.bindings()?
            .iter()
            .find(|binding| binding.chord == *chord)
            .map(PlacementBinding::action)
    }

    /// Return the process's ARC task-ID block while the ARC bounds policy is
    /// active, and `None` while it is off.
    pub(crate) fn arc_id_allocator(&self) -> Option<&'static ArcIdAllocator> {
        self.arc_bounds.as_ref().map(|policy| policy.arc_ids)
    }

    /// Return the current bindings, or `None` while the policy is off.
    fn bindings(&self) -> Option<Bindings> {
        let policy = self.arc_bounds.as_ref()?;
        // A writer only swaps in a complete `Arc`, so a poisoned lock still
        // holds a whole generation.
        Some(Arc::clone(
            &policy
                .bindings
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        ))
    }

    /// Re-read the configuration file and publish it only if it is valid.
    ///
    /// # Returns
    ///
    /// `None` when no file is in use; there is nothing to reload. That is
    /// the case without `--window-shortcuts-config`, while the policy is off,
    /// and when startup could not use the file at all (see
    /// [`BindingSource::load`]).
    ///
    /// # Errors
    ///
    /// Returns the load error and leaves the published bindings untouched.
    pub(crate) fn reload(&self) -> Option<Result<(), BindingsError>> {
        let policy = self.arc_bounds.as_ref()?;
        let path = policy.reload_path.as_deref()?;
        Some(
            load_shortcut_config(path, &self.host_accelerators).map(|bindings| {
                *policy
                    .bindings
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Bindings::from(bindings);
            }),
        )
    }

    /// Handle a reload request (`SIGHUP`) and log its outcome.
    pub(crate) fn reload_and_log(&self) {
        match self.reload() {
            None => log::info!("SIGHUP ignored: no window shortcut configuration file is in use"),
            Some(Ok(())) => self.log_loaded("Reloaded"),
            Some(Err(error)) => log::error!(
                "Keeping the last known-good window placement shortcuts; reload failed: {error}"
            ),
        }
    }

    fn log_loaded(&self, verb: &str) {
        let Some(policy) = &self.arc_bounds else {
            return;
        };
        let count = self.bindings().map_or(0, |bindings| bindings.len());
        match &policy.reload_path {
            Some(path) => log::info!(
                "{verb} {count} window placement shortcut binding(s) from {}; send SIGHUP to reload",
                path.display()
            ),
            None => log::info!("{verb} {count} window placement shortcut binding(s) from {SHORTCUTS_VARIABLE}"),
        }
    }
}

/// Use the host accelerators when no binding source depends on them.
///
/// A malformed `SOMMELIER_ACCELERATORS` must not stop the proxy when it only
/// filters keys: that would break every app in the container. It degrades to
/// no filtering, logged once.
fn host_accelerators_or_unfiltered(
    host_accelerators: Result<Vec<Accelerator>, PlacementConfigError>,
) -> Vec<Accelerator> {
    host_accelerators.unwrap_or_else(|error| {
        log::warn!("{error}. Accelerator filtering disabled.");
        Vec::new()
    })
}

/// One binding as written in either source, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    chord: String,
    action: Option<String>,
    rect: Option<[f64; 4]>,
}

/// Validate bindings from either source. This is the only validator.
///
/// # Arguments
///
/// * `raw` - the bindings in source order.
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
        return Err(BindingsError::TooManyBindings {
            count: raw.len(),
            limit: MAX_BINDINGS,
        });
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

/// Parse and validate the `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` syntax.
///
/// The value is a comma-separated list of `CHORD=ACTION` pairs, for example
/// `<Alt>q=top-left,<Alt>w=top`. Empty entries are ignored for consistency
/// with `SOMMELIER_ACCELERATORS`.
///
/// # Errors
///
/// Returns [`BindingsError::MissingAction`] for an entry without `=`, and
/// otherwise the [`validate_bindings`] error.
fn parse_environment_bindings(
    value: &str,
    host_accelerators: &[Accelerator],
) -> Result<Vec<PlacementBinding>, BindingsError> {
    let raw = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .enumerate()
        .map(|(index, entry)| {
            let (chord, action) =
                entry
                    .split_once('=')
                    .ok_or_else(|| BindingsError::MissingAction {
                        index,
                        entry: entry.to_string(),
                    })?;
            Ok(RawBinding {
                chord: chord.trim().to_string(),
                action: Some(action.trim().to_string()),
                rect: None,
            })
        })
        .collect::<Result<Vec<_>, BindingsError>>()?;
    validate_bindings(raw, host_accelerators)
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
/// disables every placement shortcut.
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
        .map_err(|source| BindingsError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = file.metadata().map_err(|source| BindingsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(BindingsError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    // Bound the read itself instead of trusting the size reported by stat;
    // the extra byte distinguishes an at-limit file from an oversized one.
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| BindingsError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(BindingsError::TooLarge {
            path: path.to_path_buf(),
            limit: MAX_CONFIG_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| BindingsError::NotUtf8 {
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
fn load_shortcut_config(
    path: &Path,
    host_accelerators: &[Accelerator],
) -> Result<Vec<PlacementBinding>, BindingsError> {
    parse_shortcut_config(&read_config_text(path)?, host_accelerators)
}

#[cfg(test)]
impl PlacementConfig {
    /// The ARC bounds policy off, with the given host accelerators.
    pub(crate) fn for_test(host_accelerators: Vec<Accelerator>) -> Self {
        Self {
            host_accelerators,
            arc_bounds: None,
        }
    }

    /// Resolve named test variables with the production rules.
    ///
    /// `arc_ids` stands in for the process block, so a test never reserves
    /// a real one.
    pub(crate) fn resolve_for_test(
        variables: &[(&str, &str)],
        shortcuts_file: Option<&Path>,
        arc_ids: &'static ArcIdAllocator,
    ) -> Self {
        let mut environment = PlacementEnvironment::default();
        for &(name, value) in variables {
            let slot = match name {
                ARC_BOUNDS_VARIABLE => &mut environment.arc_bounds,
                HOST_ACCELERATORS_VARIABLE => &mut environment.host_accelerators,
                SHORTCUTS_VARIABLE => &mut environment.shortcuts,
                other => panic!("{other} does not configure placement"),
            };
            *slot = Some(OsString::from(value));
        }
        Self::resolve(&environment, shortcuts_file, || Some(arc_ids))
    }

    /// The ARC bounds policy on, with a fresh test block and
    /// `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`-syntax bindings.
    pub(crate) fn arc_bounds_for_test(shortcuts: &str) -> Self {
        let config = Self::resolve_for_test(
            &[(ARC_BOUNDS_VARIABLE, "1"), (SHORTCUTS_VARIABLE, shortcuts)],
            None,
            ArcIdAllocator::block_for_test(),
        );
        assert!(
            shortcuts.trim().is_empty() || !config.bindings().unwrap_or_default().is_empty(),
            "invalid test bindings {shortcuts:?} would leave shortcuts disabled"
        );
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accelerator::ALT_MASK;
    use crate::test_support::TempDir;
    use std::os::unix::ffi::OsStringExt;
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

    fn environment(
        arc_bounds: Option<&str>,
        host_accelerators: Option<&str>,
        shortcuts: Option<&str>,
    ) -> PlacementEnvironment {
        PlacementEnvironment {
            arc_bounds: arc_bounds.map(OsString::from),
            host_accelerators: host_accelerators.map(OsString::from),
            shortcuts: shortcuts.map(OsString::from),
        }
    }

    /// Resolve with a test block, recording whether one was requested.
    fn resolve(
        environment: &PlacementEnvironment,
        file: Option<&Path>,
        arc_ids: Option<&'static ArcIdAllocator>,
    ) -> (PlacementConfig, bool) {
        let mut reserved = false;
        let config = PlacementConfig::resolve(environment, file, || {
            reserved = true;
            arc_ids
        });
        (config, reserved)
    }

    fn block() -> Option<&'static ArcIdAllocator> {
        Some(ArcIdAllocator::block_for_test())
    }

    fn current(config: &PlacementConfig) -> Bindings {
        config.bindings().expect("ARC bounds policy is active")
    }

    fn reload_path(config: &PlacementConfig) -> Option<&Path> {
        config.arc_bounds.as_ref()?.reload_path.as_deref()
    }

    /// The failure policy: the ARC policy stays active, but no chord is
    /// bound, so none is consumed.
    fn assert_shortcuts_disabled(config: &PlacementConfig) {
        assert!(
            config.arc_id_allocator().is_some(),
            "the ARC policy stays on"
        );
        assert!(current(config).is_empty(), "no binding is active");
        assert_eq!(config.action_for(&alt(xkb::keysyms::KEY_q)), None);
    }

    // ── One validator for both sources ────────────────────────────────

    #[test]
    fn parses_rect_and_named_bindings_in_document_order() {
        let bindings = parse(VALID).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].chord(), alt(xkb::keysyms::KEY_q));
        assert_eq!(
            bindings[0].action(),
            WindowLayoutAction::Rect(NormalizedRect::new(0.0, 0.0, 0.5, 0.5).unwrap())
        );
        assert_eq!(bindings[1].action(), WindowLayoutAction::Fullscreen);
    }

    #[test]
    fn environment_bindings_parse_in_order() {
        let bindings =
            parse_environment_bindings("<Alt>q=top-left,<Alt>w=top,<Alt>s=fullscreen", &[])
                .unwrap();
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].chord(), alt(xkb::keysyms::KEY_q));
        assert_eq!(bindings[0].action(), WindowLayoutAction::TopLeft);
        assert_eq!(bindings[2].action(), WindowLayoutAction::Fullscreen);
        assert_eq!(
            parse_environment_bindings(" , <Alt>q = left ,, ", &[]).unwrap()[0].action(),
            WindowLayoutAction::Left,
            "empty entries and padding are ignored"
        );
    }

    #[test]
    fn environment_bindings_reject_bad_syntax_duplicates_and_actions() {
        assert!(matches!(
            parse_environment_bindings(",<Alt>q", &[]),
            Err(BindingsError::MissingAction { index: 0, .. })
        ));
        assert!(matches!(
            parse_environment_bindings("<Alt>q=tile", &[]),
            Err(BindingsError::UnknownAction { index: 0, .. })
        ));
        assert!(matches!(
            parse_environment_bindings("<Alt>q=top,<Meta>Q=left", &[]),
            Err(BindingsError::DuplicateChord {
                index: 1,
                first: 0,
                ..
            })
        ));
        assert!(matches!(
            parse_environment_bindings("<Hyper>q=top", &[]),
            Err(BindingsError::InvalidChord { index: 0, .. })
        ));
    }

    #[test]
    fn both_sources_reject_host_accelerator_conflicts() {
        let host = vec![parse_accelerator("<Alt>q").unwrap()];
        assert!(matches!(
            parse_shortcut_config(&one_binding("action = \"left\""), &host),
            Err(BindingsError::HostAcceleratorConflict { index: 0, .. })
        ));
        assert!(matches!(
            parse_environment_bindings("<Alt>w=right,<Alt>q=left", &host),
            Err(BindingsError::HostAcceleratorConflict { index: 1, .. })
        ));
    }

    #[test]
    fn accepts_integer_rect_components_and_empty_binding_list() {
        let bindings = parse(&one_binding("rect = [0, 0, 1, 1]")).unwrap();
        assert_eq!(
            bindings[0].action(),
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
    fn rejects_too_many_bindings_from_either_source() {
        let mut text = String::from("version = 1\n");
        for _ in 0..=MAX_BINDINGS {
            text.push_str("[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n");
        }
        assert!(matches!(
            parse(&text),
            Err(BindingsError::TooManyBindings { count, .. }) if count == MAX_BINDINGS + 1
        ));
        let list = vec!["<Alt>q=left"; MAX_BINDINGS + 1].join(",");
        assert!(matches!(
            parse_environment_bindings(&list, &[]),
            Err(BindingsError::TooManyBindings { count, .. }) if count == MAX_BINDINGS + 1
        ));
    }

    #[test]
    fn load_rejects_oversized_non_utf8_missing_and_non_regular_files() {
        let dir = TempDir::new("shortcut-load");
        let oversized = dir.write("big.toml", &vec![b'#'; MAX_CONFIG_BYTES as usize + 1]);
        assert!(matches!(
            load_shortcut_config(&oversized, &[]),
            Err(BindingsError::TooLarge { .. })
        ));
        let mut at_limit = b"version = 1\n".to_vec();
        at_limit.resize(MAX_CONFIG_BYTES as usize, b' ');
        let at_limit = dir.write("limit.toml", &at_limit);
        assert!(load_shortcut_config(&at_limit, &[]).unwrap().is_empty());
        let binary = dir.write("binary.toml", b"version = 1\n# \xff\n");
        assert!(matches!(
            load_shortcut_config(&binary, &[]),
            Err(BindingsError::NotUtf8 { .. })
        ));
        assert!(matches!(
            load_shortcut_config(&dir.path().join("missing.toml"), &[]),
            Err(BindingsError::Open { .. })
        ));
        assert!(matches!(
            load_shortcut_config(dir.path(), &[]),
            Err(BindingsError::NotRegularFile { .. })
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
                Err(BindingsError::NotRegularFile { .. })
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

    // ── Environment rules ─────────────────────────────────────────────

    #[test]
    fn arc_bounds_gate_requires_an_explicit_true_spelling() {
        for on in ["1", "true", " YES ", "on"] {
            assert!(environment(Some(on), None, None).arc_bounds_requested());
        }
        for off in ["", "0", "false", "1please"] {
            assert!(!environment(Some(off), None, None).arc_bounds_requested());
        }
        assert!(!environment(None, None, None).arc_bounds_requested());
        let not_unicode = PlacementEnvironment {
            arc_bounds: Some(OsString::from_vec(vec![b'1', 0xff])),
            ..PlacementEnvironment::default()
        };
        assert!(!not_unicode.arc_bounds_requested());
    }

    #[test]
    fn blank_means_unset_and_non_utf8_means_malformed() {
        assert_eq!(configured_text(None, "V").unwrap(), None);
        assert_eq!(configured_text(Some(OsStr::new("")), "V").unwrap(), None);
        assert_eq!(configured_text(Some(OsStr::new(" \t")), "V").unwrap(), None);
        assert_eq!(
            configured_text(Some(OsStr::new("<Alt>q=left")), "V").unwrap(),
            Some("<Alt>q=left")
        );
        let not_unicode = OsString::from_vec(vec![0xff]);
        assert!(matches!(
            configured_text(Some(&not_unicode), "V"),
            Err(PlacementConfigError::NotUnicode { variable: "V" })
        ));
    }

    #[test]
    fn host_accelerators_are_unset_valid_or_invalid() {
        assert_eq!(
            environment(None, None, None).host_accelerators().unwrap(),
            Vec::new()
        );
        assert_eq!(
            environment(None, Some("  "), None)
                .host_accelerators()
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            environment(None, Some("<Alt>a"), None)
                .host_accelerators()
                .unwrap(),
            vec![parse_accelerator("<Alt>a").unwrap()]
        );
        assert!(matches!(
            environment(None, Some("<Hyper>a"), None).host_accelerators(),
            Err(PlacementConfigError::InvalidHostAccelerators { .. })
        ));
    }

    // ── Resolution and failure policy ─────────────────────────────────

    #[test]
    fn gate_off_ignores_both_sources_without_reading_or_reserving() {
        let dir = TempDir::new("placement-gate-off");
        for (env, file) in [
            (
                environment(None, None, Some("<Alt>q=bogus")),
                None::<PathBuf>,
            ),
            (
                environment(Some("0"), None, None),
                Some(dir.path().join("missing.toml")),
            ),
        ] {
            let (config, reserved) = resolve(&env, file.as_deref(), block());
            assert!(!reserved, "the gate decides whether a block is reserved");
            assert!(config.arc_id_allocator().is_none());
            assert_eq!(config.action_for(&alt(xkb::keysyms::KEY_q)), None);
            assert!(config.reload().is_none());
        }
    }

    #[test]
    fn no_arc_block_ignores_both_sources_without_reading() {
        let dir = TempDir::new("placement-no-block");
        let missing = dir.path().join("missing.toml");
        for (env, file) in [
            (environment(Some("1"), None, Some("<Alt>q=bogus")), None),
            (environment(Some("1"), None, None), Some(missing.as_path())),
        ] {
            let (config, reserved) = resolve(&env, file, None);
            assert!(reserved);
            assert!(config.arc_id_allocator().is_none());
            assert!(config.reload().is_none());
        }
    }

    /// Both sources at once used to stop startup with exit status 2; now
    /// the conflict is logged and shortcuts start disabled.
    #[test]
    fn both_sources_at_once_disable_shortcuts() {
        let dir = TempDir::new("placement-both");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());
        let (config, _) = resolve(
            &environment(Some("1"), None, Some("<Alt>q=left")),
            Some(&path),
            block(),
        );
        assert_shortcuts_disabled(&config);
        assert!(config.reload().is_none(), "neither source is followed");
        assert!(matches!(
            BindingSource::Both(&path).load(&[]),
            (None, Err(PlacementConfigError::ConflictingSources))
        ));
        // With the gate off, both are ignored like any source.
        let (config, _) = resolve(
            &environment(None, None, Some("<Alt>q=left")),
            Some(&path),
            block(),
        );
        assert!(config.arc_id_allocator().is_none());
        // A blank environment list is not a second source.
        let (config, _) = resolve(
            &environment(Some("1"), None, Some(" ")),
            Some(&path),
            block(),
        );
        assert_eq!(current(&config).len(), 2);
    }

    /// An invalid environment list used to log a warning on every
    /// connection. Now it is logged once per process and shortcuts start
    /// disabled, exactly like an unusable file.
    #[test]
    fn invalid_environment_bindings_disable_shortcuts() {
        for (host, shortcuts) in [
            (None, "<Alt>q=tile"),
            (None, "<Alt>q"),
            (None, "<Alt>q=top,<Meta>Q=left"),
            (Some("<Alt>q"), "<Alt>q=left"),
        ] {
            let (config, _) = resolve(
                &environment(Some("1"), host, Some(shortcuts)),
                None,
                block(),
            );
            assert_shortcuts_disabled(&config);
            assert!(config.reload().is_none(), "the list never reloads");
            let host = host.map_or_else(Vec::new, |host| parse_accelerators(host).unwrap());
            assert!(
                matches!(
                    BindingSource::Environment(Ok(shortcuts)).load(&host),
                    (None, Err(PlacementConfigError::Environment(_)))
                ),
                "{shortcuts:?}"
            );
        }
        let not_unicode = PlacementEnvironment {
            arc_bounds: Some("1".into()),
            shortcuts: Some(OsString::from_vec(vec![0xff])),
            ..PlacementEnvironment::default()
        };
        assert_shortcuts_disabled(&resolve(&not_unicode, None, block()).0);
        assert!(matches!(
            configured_text(not_unicode.shortcuts.as_deref(), SHORTCUTS_VARIABLE),
            Err(PlacementConfigError::NotUnicode {
                variable: SHORTCUTS_VARIABLE
            })
        ));
    }

    #[test]
    fn invalid_host_accelerators_disable_a_requested_source_and_filtering() {
        let dir = TempDir::new("placement-host");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());
        let alt_a = parse_accelerator("<Alt>a").unwrap();
        for (shortcuts, file) in [(Some("<Alt>q=left"), None), (None, Some(path.as_path()))] {
            let (config, _) = resolve(
                &environment(Some("1"), Some("<Hyper>a"), shortcuts),
                file,
                block(),
            );
            assert_shortcuts_disabled(&config);
            assert!(!config.is_host_accelerator(&alt_a));
            assert!(
                config.reload().is_none(),
                "a reload could not be checked against the host list either"
            );
        }
        // Without a binding source the list only filters keys, so it
        // degrades to no filtering.
        for gate in [None, Some("1")] {
            let (config, _) = resolve(&environment(gate, Some("<Hyper>a"), None), None, block());
            assert!(!config.is_host_accelerator(&alt_a));
        }
    }

    #[test]
    fn environment_source_resolves_bindings_and_never_reloads() {
        let (config, reserved) = resolve(
            &environment(Some("1"), Some("<Alt>a"), Some("<Alt>q=left")),
            None,
            block(),
        );
        assert!(reserved);
        assert!(config.arc_id_allocator().is_some());
        assert!(config.is_host_accelerator(&alt(xkb::keysyms::KEY_a)));
        assert_eq!(
            config.action_for(&alt(xkb::keysyms::KEY_q)),
            Some(WindowLayoutAction::Left)
        );
        assert_eq!(config.action_for(&alt(xkb::keysyms::KEY_w)), None);
        assert!(config.reload().is_none());
        config.reload_and_log();
    }

    #[test]
    fn active_policy_without_a_source_consumes_no_chord() {
        let (config, _) = resolve(&environment(Some("1"), None, None), None, block());
        assert!(config.arc_id_allocator().is_some());
        assert!(current(&config).is_empty());
    }

    #[test]
    fn file_source_loads_strictly() {
        let dir = TempDir::new("placement-file");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());
        let gate = environment(Some("1"), None, None);
        let config = resolve(&gate, Some(&path), block()).0;
        assert_eq!(current(&config).len(), 2);
        assert_eq!(reload_path(&config), Some(path.as_path()));

        let (config, _) = resolve(
            &environment(Some("1"), Some("<Alt>s"), None),
            Some(&path),
            block(),
        );
        assert_shortcuts_disabled(&config);
        assert!(matches!(
            BindingSource::File(&path).load(&[parse_accelerator("<Alt>s").unwrap()]),
            (
                Some(_),
                Err(PlacementConfigError::File(
                    BindingsError::HostAcceleratorConflict { index: 1, .. }
                ))
            )
        ));
    }

    /// A missing, unreadable or invalid file used to stop startup with exit
    /// status 2. Now shortcuts start disabled, and because the path is
    /// known, a later valid reload activates the file.
    #[test]
    fn unusable_file_disables_shortcuts_until_a_valid_reload() {
        let dir = TempDir::new("placement-file-invalid");
        let path = dir.path().join("shortcuts.toml");
        let (config, _) = resolve(&environment(Some("1"), None, None), Some(&path), block());
        assert_shortcuts_disabled(&config);
        assert_eq!(reload_path(&config), Some(path.as_path()));
        assert!(matches!(
            config.reload(),
            Some(Err(BindingsError::Open { .. }))
        ));

        dir.write("shortcuts.toml", b"not toml");
        let (config, _) = resolve(&environment(Some("1"), None, None), Some(&path), block());
        assert_shortcuts_disabled(&config);

        dir.write("shortcuts.toml", VALID.as_bytes());
        config.reload().unwrap().unwrap();
        assert_eq!(current(&config).len(), 2);
        assert_eq!(
            config.action_for(&alt(xkb::keysyms::KEY_s)),
            Some(WindowLayoutAction::Fullscreen)
        );
    }

    #[test]
    fn file_source_resolves_relative_paths_against_the_startup_directory() {
        let relative = Path::new("sommelier-shortcuts-that-do-not-exist.toml");
        let absolute = std::env::current_dir().unwrap().join(relative);
        match BindingSource::File(relative).load(&[]) {
            (
                Some(reload_path),
                Err(PlacementConfigError::File(BindingsError::Open { path, .. })),
            ) => {
                assert_eq!(path, absolute);
                assert_eq!(reload_path, absolute);
            }
            other => panic!("expected absolute open failure, got {other:?}"),
        }
    }

    // ── Reload ────────────────────────────────────────────────────────

    #[test]
    fn reload_swaps_valid_generation_and_keeps_last_good_on_error() {
        let dir = TempDir::new("shortcut-reload");
        let path = dir.write("shortcuts.toml", VALID.as_bytes());
        let config = PlacementConfig::resolve_for_test(
            &[(ARC_BOUNDS_VARIABLE, "1")],
            Some(&path),
            ArcIdAllocator::block_for_test(),
        );
        assert_eq!(current(&config).len(), 2);

        dir.write(
            "shortcuts.toml",
            one_binding("action = \"left\"").as_bytes(),
        );
        config.reload().unwrap().unwrap();
        assert_eq!(current(&config).len(), 1);
        assert_eq!(
            config.action_for(&alt(xkb::keysyms::KEY_q)),
            Some(WindowLayoutAction::Left),
            "the next lookup sees the new generation"
        );
        assert_eq!(config.action_for(&alt(xkb::keysyms::KEY_s)), None);

        let last_good = current(&config);
        for invalid in [
            one_binding("rect = [0.8, 0.0, 0.5, 0.5]"),
            "version = 1\n[[bindings]]\nchord = \"<Alt>q\"\n".to_string(),
            "not toml".to_string(),
        ] {
            dir.write("shortcuts.toml", invalid.as_bytes());
            assert!(config.reload().unwrap().is_err());
            config.reload_and_log();
            assert!(Arc::ptr_eq(&last_good, &current(&config)));
        }

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            config.reload(),
            Some(Err(BindingsError::Open { .. }))
        ));
        assert!(Arc::ptr_eq(&last_good, &current(&config)));

        dir.write("shortcuts.toml", b"version = 1\n");
        config.reload().unwrap().unwrap();
        config.reload_and_log();
        assert!(current(&config).is_empty());
    }

    #[test]
    fn reload_keeps_rejecting_startup_host_accelerators() {
        let dir = TempDir::new("shortcut-reload-host");
        let path = dir.write("shortcuts.toml", b"version = 1\n");
        let config = PlacementConfig::resolve_for_test(
            &[
                (ARC_BOUNDS_VARIABLE, "1"),
                (HOST_ACCELERATORS_VARIABLE, "<Alt>q"),
            ],
            Some(&path),
            ArcIdAllocator::block_for_test(),
        );
        dir.write(
            "shortcuts.toml",
            one_binding("action = \"left\"").as_bytes(),
        );
        assert!(matches!(
            config.reload(),
            Some(Err(BindingsError::HostAcceleratorConflict { .. }))
        ));
        assert!(current(&config).is_empty());
    }
}
