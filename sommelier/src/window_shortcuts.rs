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

//! Parsing and runtime ownership for compositor-owned window shortcuts.
//!
//! The config is intentionally immutable after parsing. Runtime reloads build
//! a complete replacement and publish it through [`ShortcutConfigHandle`] only
//! after every binding, geometry, and host-accelerator conflict has passed
//! validation.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;
use thiserror::Error;

use crate::accelerator::{parse_accelerator, Accelerator};

const CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_BINDINGS: usize = 256;

/// A rectangle expressed as fractions of the active output work area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NormalizedRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl NormalizedRect {
    /// Construct a rectangle in normalized work-area coordinates.
    pub(crate) const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Convert this rectangle to screen-space bounds.
    ///
    /// Work-area coordinates are `(origin_x, origin_y, width, height)`.
    /// Rounding both endpoints from the normalized coordinate keeps adjacent
    /// regions sharing the same integer boundary, including odd-sized output
    /// modes.
    pub(crate) fn to_bounds(self, work_area: (i32, i32, i32, i32)) -> Option<(i32, i32, i32, i32)> {
        let (origin_x, origin_y, work_width, work_height) = work_area;
        if work_width <= 0 || work_height <= 0 {
            return None;
        }
        let values = [self.x, self.y, self.width, self.height];
        if !values.iter().all(|value| value.is_finite())
            || self.x < 0.0
            || self.y < 0.0
            || self.width <= 0.0
            || self.height <= 0.0
            || self.x + self.width > 1.0
            || self.y + self.height > 1.0
        {
            return None;
        }

        let scale = |value: f64, total: i32| -> Option<i64> {
            let scaled = value * f64::from(total);
            scaled
                .is_finite()
                .then(|| scaled.round())
                .filter(|rounded| *rounded >= i64::MIN as f64 && *rounded <= i64::MAX as f64)
                .map(|rounded| rounded as i64)
        };

        let left = scale(self.x, work_width)?;
        let right = scale(self.x + self.width, work_width)?;
        let top = scale(self.y, work_height)?;
        let bottom = scale(self.y + self.height, work_height)?;
        let width = right.checked_sub(left)?;
        let height = bottom.checked_sub(top)?;
        if width <= 0 || height <= 0 {
            return None;
        }

        let absolute_x = i64::from(origin_x).checked_add(left)?;
        let absolute_y = i64::from(origin_y).checked_add(top)?;
        Some((
            i32::try_from(absolute_x).ok()?,
            i32::try_from(absolute_y).ok()?,
            i32::try_from(width).ok()?,
            i32::try_from(height).ok()?,
        ))
    }
}

/// One validated `window.place` binding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowShortcut {
    pub(crate) accelerator: Accelerator,
    pub(crate) rect: NormalizedRect,
}

/// A complete, immutable shortcut configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ShortcutConfig {
    bindings: Vec<WindowShortcut>,
}

impl ShortcutConfig {
    /// Return an empty configuration that consumes no keyboard shortcuts.
    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Return whether this configuration contains no bindings.
    pub(crate) fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Find the binding for an exact normalized accelerator.
    pub(crate) fn find(&self, accelerator: Accelerator) -> Option<WindowShortcut> {
        self.bindings
            .iter()
            .find(|binding| binding.accelerator == accelerator)
            .copied()
    }

    /// Parse and validate a complete TOML configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the document, binding chord, action, rectangle,
    /// duplicate set, or host-accelerator conflict is invalid.
    pub(crate) fn parse(
        text: &str,
        host_accelerators: &[Accelerator],
    ) -> Result<Arc<Self>, ShortcutConfigError> {
        let raw: RawConfig = toml::from_str(text)?;
        if raw.version != CONFIG_VERSION {
            return Err(ShortcutConfigError::UnsupportedVersion(raw.version));
        }
        if raw.bindings.len() > MAX_BINDINGS {
            return Err(ShortcutConfigError::TooManyBindings {
                actual: raw.bindings.len(),
                maximum: MAX_BINDINGS,
            });
        }

        let mut bindings = Vec::with_capacity(raw.bindings.len());
        for (index, raw_binding) in raw.bindings.into_iter().enumerate() {
            let accelerator = parse_accelerator(&raw_binding.chord).map_err(|error| {
                ShortcutConfigError::InvalidBinding {
                    index,
                    reason: format!("invalid chord {:?}: {error}", raw_binding.chord),
                }
            })?;
            if bindings
                .iter()
                .any(|binding: &WindowShortcut| binding.accelerator == accelerator)
            {
                return Err(ShortcutConfigError::DuplicateChord {
                    index,
                    chord: raw_binding.chord,
                });
            }
            if host_accelerators.contains(&accelerator) {
                return Err(ShortcutConfigError::HostAcceleratorConflict {
                    index,
                    chord: raw_binding.chord,
                });
            }
            if raw_binding.action != "window.place" {
                return Err(ShortcutConfigError::InvalidBinding {
                    index,
                    reason: format!(
                        "unsupported action {:?}; only \"window.place\" is supported",
                        raw_binding.action
                    ),
                });
            }

            let [x, y, width, height] = raw_binding.rect;
            let rect = NormalizedRect::new(x, y, width, height);
            if ![x, y, width, height].iter().all(|value| value.is_finite()) {
                return Err(ShortcutConfigError::InvalidBinding {
                    index,
                    reason: "rect values must be finite".to_string(),
                });
            }
            if !(0.0..=1.0).contains(&x)
                || !(0.0..=1.0).contains(&y)
                || !(0.0..=1.0).contains(&width)
                || !(0.0..=1.0).contains(&height)
                || width <= 0.0
                || height <= 0.0
                || x + width > 1.0
                || y + height > 1.0
            {
                return Err(ShortcutConfigError::InvalidBinding {
                    index,
                    reason: format!(
                        "rect must satisfy 0 <= x,y,width,height <= 1, \
                         width/height > 0, x+width <= 1, y+height <= 1; got {:?}",
                        raw_binding.rect
                    ),
                });
            }

            bindings.push(WindowShortcut { accelerator, rect });
        }

        Ok(Arc::new(Self { bindings }))
    }

    /// Read and parse a configuration file.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem, UTF-8, TOML, or binding validation
    /// failures. The caller owns the decision to retain a previous generation.
    pub(crate) fn load_from_path(
        path: &Path,
        host_accelerators: &[Accelerator],
    ) -> Result<Arc<Self>, ShortcutConfigError> {
        let file = fs::File::open(path).map_err(|source| ShortcutConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Bound the read itself rather than loading an untrusted path with
        // `fs::read` and checking its size afterwards. The extra byte lets us
        // distinguish an exactly-at-limit file from one that exceeds it.
        let mut bytes = Vec::with_capacity(
            MAX_CONFIG_BYTES.min(
                file.metadata()
                    .map(|metadata| metadata.len() as usize)
                    .unwrap_or_default()
                    .saturating_add(1),
            ),
        );
        file.take((MAX_CONFIG_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| ShortcutConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ShortcutConfigError::FileTooLarge {
                actual: bytes.len(),
                maximum: MAX_CONFIG_BYTES,
            });
        }
        let text = String::from_utf8(bytes).map_err(|source| ShortcutConfigError::InvalidUtf8 {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, host_accelerators)
    }

    #[cfg(test)]
    pub(crate) fn test_nine_grid() -> Arc<Self> {
        Self::parse(
            r#"
version = 1
[[bindings]]
chord = "<Alt>q"
action = "window.place"
rect = [0.0, 0.0, 0.5, 0.5]
[[bindings]]
chord = "<Alt>w"
action = "window.place"
rect = [0.0, 0.0, 1.0, 0.5]
[[bindings]]
chord = "<Alt>e"
action = "window.place"
rect = [0.5, 0.0, 0.5, 0.5]
[[bindings]]
chord = "<Alt>a"
action = "window.place"
rect = [0.0, 0.0, 0.5, 1.0]
[[bindings]]
chord = "<Alt>s"
action = "window.place"
rect = [0.0, 0.0, 1.0, 1.0]
[[bindings]]
chord = "<Alt>d"
action = "window.place"
rect = [0.5, 0.0, 0.5, 1.0]
[[bindings]]
chord = "<Alt>z"
action = "window.place"
rect = [0.0, 0.5, 0.5, 0.5]
[[bindings]]
chord = "<Alt>x"
action = "window.place"
rect = [0.0, 0.5, 1.0, 0.5]
[[bindings]]
chord = "<Alt>c"
action = "window.place"
rect = [0.5, 0.5, 0.5, 0.5]
"#,
            &[],
        )
        .expect("nine-grid fixture must be valid")
    }
}

/// Shared immutable configuration handle used by all client connections.
#[derive(Clone, Debug)]
pub(crate) struct ShortcutConfigHandle {
    current: Arc<RwLock<Arc<ShortcutConfig>>>,
}

impl ShortcutConfigHandle {
    /// Publish an immutable configuration generation.
    pub(crate) fn new(config: Arc<ShortcutConfig>) -> Self {
        Self {
            current: Arc::new(RwLock::new(config)),
        }
    }

    /// Return a handle containing no active bindings.
    pub(crate) fn disabled() -> Self {
        Self::new(ShortcutConfig::disabled())
    }

    /// Clone the currently published immutable configuration.
    pub(crate) fn snapshot(&self) -> Arc<ShortcutConfig> {
        match self.current.read() {
            Ok(config) => config.clone(),
            Err(poisoned) => {
                // The protected value is an immutable Arc and remains valid
                // even if a future maintenance change panics while holding
                // the lock. Recover the last complete generation instead of
                // taking down the compositor on the next key event.
                log::error!("shortcut config lock poisoned; recovering last generation");
                poisoned.into_inner().clone()
            }
        }
    }

    /// Replace the published generation after validation has succeeded.
    pub(crate) fn replace(&self, config: Arc<ShortcutConfig>) {
        match self.current.write() {
            Ok(mut current) => *current = config,
            Err(poisoned) => {
                log::error!("shortcut config lock poisoned; replacing recovered generation");
                *poisoned.into_inner() = config;
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    #[serde(default)]
    bindings: Vec<RawBinding>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBinding {
    chord: String,
    action: String,
    rect: [f64; 4],
}

#[derive(Debug, Error)]
pub(crate) enum ShortcutConfigError {
    #[error("unable to read shortcut config {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("shortcut config {path} is not valid UTF-8: {source}")]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("invalid shortcut TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("unsupported shortcut config version {0}; expected {CONFIG_VERSION}")]
    UnsupportedVersion(u32),
    #[error("shortcut config contains {actual} bindings; maximum is {maximum}")]
    TooManyBindings { actual: usize, maximum: usize },
    #[error("binding {index}: {reason}")]
    InvalidBinding { index: usize, reason: String },
    #[error("binding {index} duplicates chord {chord:?}")]
    DuplicateChord { index: usize, chord: String },
    #[error("binding {index} chord {chord:?} conflicts with SOMMELIER_ACCELERATORS")]
    HostAcceleratorConflict { index: usize, chord: String },
    #[error("shortcut config is {actual} bytes; maximum is {maximum} bytes")]
    FileTooLarge { actual: usize, maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accelerator::{parse_accelerator, ALT_MASK};

    #[test]
    fn parses_inline_binding_and_matches_accelerator() {
        let config = ShortcutConfig::parse(
            r#"
version = 1
[[bindings]]
chord = "<Alt>q"
action = "window.place"
rect = [0.0, 0.0, 0.5, 0.5]
"#,
            &[],
        )
        .unwrap();
        assert_eq!(config.bindings.len(), 1);
        assert_eq!(
            config
                .find(Accelerator {
                    modifiers: ALT_MASK,
                    symbol: xkbcommon::xkb::keysyms::KEY_q,
                })
                .unwrap()
                .rect,
            NormalizedRect::new(0.0, 0.0, 0.5, 0.5)
        );
    }

    #[test]
    fn rejects_duplicate_action_chords() {
        let error = ShortcutConfig::parse(
            r#"
version = 1
[[bindings]]
chord = "<Alt>q"
action = "window.place"
rect = [0.0, 0.0, 0.5, 0.5]
[[bindings]]
chord = "<Meta>q"
action = "window.place"
rect = [0.0, 0.0, 0.5, 0.5]
"#,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ShortcutConfigError::DuplicateChord { index: 1, .. }
        ));
    }

    #[test]
    fn rejects_host_accelerator_overlap() {
        let host = vec![parse_accelerator("<Alt>q").unwrap()];
        let error = ShortcutConfig::parse(
            r#"
version = 1
[[bindings]]
chord = "<Alt>q"
action = "window.place"
rect = [0.0, 0.0, 0.5, 0.5]
"#,
            &host,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ShortcutConfigError::HostAcceleratorConflict { index: 0, .. }
        ));
    }

    #[test]
    fn rejects_unknown_action_and_invalid_rect() {
        let error = ShortcutConfig::parse(
            r#"
version = 1
[[bindings]]
chord = "<Alt>q"
action = "window.move"
rect = [0.0, 0.0, 0.5, 0.5]
"#,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ShortcutConfigError::InvalidBinding { index: 0, .. }
        ));

        let error = ShortcutConfig::parse(
            r#"
version = 1
[[bindings]]
chord = "<Alt>q"
action = "window.place"
rect = [0.8, 0.0, 0.5, 0.5]
"#,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ShortcutConfigError::InvalidBinding { index: 0, .. }
        ));
    }

    #[test]
    fn rejects_unknown_version_and_fields() {
        let error = ShortcutConfig::parse("version = 2\n", &[]).unwrap_err();
        assert!(matches!(error, ShortcutConfigError::UnsupportedVersion(2)));

        let error = ShortcutConfig::parse("version = 1\nextra = true\n", &[]).unwrap_err();
        assert!(matches!(error, ShortcutConfigError::Toml(_)));
    }

    #[test]
    fn rejects_a_file_that_exceeds_the_bounded_read_limit() {
        let path = std::env::temp_dir().join(format!(
            "sommelier-window-shortcuts-too-large-{}-{}.toml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let bytes = vec![b' '; MAX_CONFIG_BYTES + 1];
        fs::write(&path, bytes).expect("write oversized shortcut config");
        let error = ShortcutConfig::load_from_path(&path, &[])
            .expect_err("oversized shortcut config must be rejected");
        let _ = fs::remove_file(&path);
        match error {
            ShortcutConfigError::FileTooLarge { actual, maximum } => {
                assert_eq!(actual, MAX_CONFIG_BYTES + 1);
                assert_eq!(maximum, MAX_CONFIG_BYTES);
            }
            other => panic!("expected bounded file-size error, got {other:?}"),
        }
    }

    #[test]
    fn converts_normalized_rect_to_work_area_bounds() {
        assert_eq!(
            NormalizedRect::new(0.5, 0.5, 0.5, 0.5).to_bounds((10, 20, 101, 99)),
            Some((61, 70, 50, 49))
        );
        assert_eq!(
            NormalizedRect::new(0.0, 0.0, 1.0, 1.0).to_bounds((0, 0, 3840, 2160)),
            Some((0, 0, 3840, 2160))
        );
    }

    #[test]
    fn rejects_zero_pixel_result_and_invalid_work_area() {
        assert_eq!(
            NormalizedRect::new(0.0, 0.0, 0.0001, 0.5).to_bounds((0, 0, 100, 100)),
            None
        );
        assert_eq!(
            NormalizedRect::new(0.0, 0.0, 1.0, 1.0).to_bounds((0, 0, 0, 100)),
            None
        );
        assert_eq!(
            NormalizedRect::new(-0.1, 0.0, 0.5, 0.5).to_bounds((0, 0, 100, 100)),
            None
        );
    }

    #[test]
    fn handle_replaces_snapshot_atomically() {
        let handle = ShortcutConfigHandle::disabled();
        assert!(handle.snapshot().is_empty());
        handle.replace(ShortcutConfig::test_nine_grid());
        assert_eq!(handle.snapshot().bindings.len(), 9);
    }

    #[test]
    fn handle_recovers_after_a_writer_panics() {
        let handle = ShortcutConfigHandle::disabled();
        let poisoned_handle = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned_handle
                .current
                .write()
                .expect("test lock should initially be healthy");
            panic!("intentional shortcut config lock poison");
        })
        .join();

        handle.replace(ShortcutConfig::test_nine_grid());
        assert_eq!(handle.snapshot().bindings.len(), 9);
    }
}
