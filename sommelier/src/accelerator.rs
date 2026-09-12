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

//! Parser for the `SOMMELIER_ACCELERATORS` environment variable.
//!
//! The variable holds a comma-separated list of key combinations that the
//! **host compositor** should handle. Each entry is a keysym name optionally
//! preceded by modifier tags:
//!
//! ```text
//! SOMMELIER_ACCELERATORS="Super_L,<Alt>bracketleft,<Alt>bracketright,<Control>space"
//! ```
//!
//! Accepted modifier tags (case-insensitive):
//!   `<Control>` / `<Ctrl>`, `<Alt>` / `<Meta>`,
//!   `<Shift>`, `<Super>` / `<Win>` / `<Search>`.
//!
//! Keys matching this list are acked as `NOT_HANDLED` via
//! `zcr_extended_keyboard_v1.ack_key`, causing the host to process the
//! accelerator. All other keys are acked as `HANDLED`, keeping them in the
//! guest.

use xkbcommon::xkb;

/// Modifier bitmask constants.
///
/// These bit positions follow the sommelier C convention, not X11's `Mod*Mask`
/// values. They are used only internally for accelerator matching.
pub(crate) const CONTROL_MASK: u32 = 1 << 0;
pub(crate) const ALT_MASK: u32 = 1 << 1;
pub(crate) const SHIFT_MASK: u32 = 1 << 2;
pub(crate) const SUPER_MASK: u32 = 1 << 3;

// We need xkb_keysym_to_lower for case-insensitive keysym matching.
// The xkbcommon-rs crate does not expose this, so we link directly.
// TODO: remove this FFI shim if xkbcommon-rs upstream adds xkb_keysym_to_lower.
#[link(name = "xkbcommon")]
extern "C" {
    #[link_name = "xkb_keysym_to_lower"]
    fn xkb_keysym_to_lower_ffi(sym: u32) -> u32;
}

/// Return the lowercase equivalent of a keysym (safe wrapper around the
/// `xkb_keysym_to_lower` C function). Pure, no side-effects.
pub(crate) fn keysym_to_lower(sym: u32) -> u32 {
    // Safety: xkb_keysym_to_lower is a pure C function with no restrictions
    // on its u32 argument — all values are valid keysyms.
    unsafe { xkb_keysym_to_lower_ffi(sym) }
}

/// A parsed accelerator: modifier bitmask + lowercase keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accelerator {
    pub modifiers: u32,
    pub symbol: u32,
}

/// Actions that may be bound to compositor-owned window placement shortcuts.
///
/// The binding is deliberately parsed from configuration rather than inferred
/// from a fixed set of keysyms in the keyboard event path.  This keeps keyboard
/// ownership policy at the process boundary and lets deployments choose a
/// layout without changing the proxy binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowLayoutAction {
    TopLeft,
    Top,
    TopRight,
    Left,
    Fullscreen,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowPlacementShortcut {
    pub(crate) accelerator: Accelerator,
    pub(crate) action: WindowLayoutAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    InvalidModifier(String),
    InvalidKeysym(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidModifier(tok) => write!(f, "Invalid modifier syntax or tag: '{}'", tok),
            Self::InvalidKeysym(sym) => write!(f, "Unknown or invalid keysym: '{}'", sym),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parses a single token into an `Accelerator`, or returns a `ParseError`.
pub(crate) fn parse_accelerator(token: &str) -> Result<Accelerator, ParseError> {
    let mut token = token.trim();
    let mut modifiers = 0;

    while token.starts_with('<') {
        let end_idx = token
            .find('>')
            .ok_or_else(|| ParseError::InvalidModifier(token.to_string()))?;
        let mod_tag = &token[..=end_idx];
        match mod_tag.to_ascii_lowercase().as_str() {
            // Accept both the full XKB names and common shorthands used in
            // C sommelier configs so that users can copy configs verbatim.
            "<control>" | "<ctrl>" => modifiers |= CONTROL_MASK,
            "<alt>" | "<meta>" => modifiers |= ALT_MASK,
            "<shift>" => modifiers |= SHIFT_MASK,
            // <Search> is the ChromeOS Launcher/Search key; it maps to Super
            // on Chromebooks. Accept it so users can copy C sommelier configs verbatim.
            // TODO: add <Hyper> if upstream ChromeOS configs ever use it.
            "<super>" | "<win>" | "<search>" => modifiers |= SUPER_MASK,
            _ => return Err(ParseError::InvalidModifier(token.to_string())),
        }
        token = &token[end_idx + 1..];
    }

    if token.is_empty() {
        return Err(ParseError::InvalidKeysym("Empty keysym".to_string()));
    }

    let sym = xkb::keysym_from_name(token, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym.raw() == xkb::keysyms::KEY_NoSymbol {
        return Err(ParseError::InvalidKeysym(token.to_string()));
    }

    Ok(Accelerator {
        modifiers,
        // Normalise to lowercase at parse time so the Accelerator is
        // self-contained; the match site (is_host_accelerator) also lowercases
        // the incoming sym, but having both sides normalised makes the
        // invariant explicit and removes an implicit coupling.
        symbol: keysym_to_lower(sym.raw()),
    })
}

/// Parse a `SOMMELIER_ACCELERATORS`-style string into a list of accelerators.
pub fn parse_accelerators(s: &str) -> Result<Vec<Accelerator>, ParseError> {
    let mut result = Vec::new();
    for token in s.split(',') {
        let token = token.trim();
        // Intentionally skip empty tokens so that trailing/double commas
        // (e.g. "Super_L,") in user-provided configs are silently tolerated
        // rather than rejected with a parse error.
        if token.is_empty() {
            continue;
        }
        result.push(parse_accelerator(token)?);
    }
    Ok(result)
}

/// Parse compositor-owned window placement bindings.
///
/// The format is a comma-separated list of `CHORD=ACTION` pairs, for example:
///
/// ```text
/// <Alt>q=top-left,<Alt>w=top,<Alt>s=fullscreen
/// ```
///
/// Empty entries are ignored for consistency with `SOMMELIER_ACCELERATORS`.
/// Duplicate chords are rejected so a deployment cannot depend on ordering.
pub(crate) fn parse_window_placement_shortcuts(
    value: &str,
) -> Result<Vec<WindowPlacementShortcut>, String> {
    let mut bindings = Vec::new();
    for (index, raw_binding) in value.split(',').enumerate() {
        let raw_binding = raw_binding.trim();
        if raw_binding.is_empty() {
            continue;
        }
        let (chord, action) = raw_binding.split_once('=').ok_or_else(|| {
            format!(
                "binding {} must use CHORD=ACTION syntax, got {:?}",
                index, raw_binding
            )
        })?;
        let accelerator = parse_accelerator(chord.trim())
            .map_err(|error| format!("binding {} has invalid chord: {}", index, error))?;
        let action_name = action.trim().to_ascii_lowercase();
        let action = match action_name.as_str() {
            "top-left" => WindowLayoutAction::TopLeft,
            "top" => WindowLayoutAction::Top,
            "top-right" => WindowLayoutAction::TopRight,
            "left" => WindowLayoutAction::Left,
            "fullscreen" => WindowLayoutAction::Fullscreen,
            "right" => WindowLayoutAction::Right,
            "bottom-left" => WindowLayoutAction::BottomLeft,
            "bottom" => WindowLayoutAction::Bottom,
            "bottom-right" => WindowLayoutAction::BottomRight,
            _ => {
                return Err(format!(
                    "binding {} has unsupported action {:?}",
                    index,
                    action.trim()
                ));
            }
        };
        if bindings
            .iter()
            .any(|binding: &WindowPlacementShortcut| binding.accelerator == accelerator)
        {
            return Err(format!(
                "binding {} duplicates accelerator {:?}",
                index,
                chord.trim()
            ));
        }
        bindings.push(WindowPlacementShortcut {
            accelerator,
            action,
        });
    }
    Ok(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accelerators_standard_list() {
        let list = parse_accelerators("Super_L,<Alt>bracketleft,<ALT>bracketright,<Control>space")
            .unwrap();
        assert_eq!(list.len(), 4);

        assert_eq!(list[0].modifiers, 0);
        assert_eq!(list[0].symbol, xkb::keysyms::KEY_Super_L);

        assert_eq!(list[1].modifiers, ALT_MASK);
        assert_eq!(list[1].symbol, xkb::keysyms::KEY_bracketleft);

        assert_eq!(list[2].modifiers, ALT_MASK);
        assert_eq!(list[2].symbol, xkb::keysyms::KEY_bracketright);

        assert_eq!(list[3].modifiers, CONTROL_MASK);
        assert_eq!(list[3].symbol, xkb::keysyms::KEY_space);
    }

    #[test]
    fn parse_empty_string() {
        assert!(parse_accelerators("").unwrap().is_empty());
    }

    #[test]
    fn parse_fails_on_invalid() {
        // Unknown keysym
        assert_eq!(
            parse_accelerators("invalid_key_name"),
            Err(ParseError::InvalidKeysym("invalid_key_name".to_string()))
        );

        // Totally unknown modifier tag (not a shorthand or full name)
        assert_eq!(
            parse_accelerators("<Hyper>a"),
            Err(ParseError::InvalidModifier("<Hyper>a".to_string()))
        );

        // Missing closing bracket
        assert_eq!(
            parse_accelerators("<Controla"),
            Err(ParseError::InvalidModifier("<Controla".to_string()))
        );

        // Multiple modifiers + missing keysym
        assert_eq!(
            parse_accelerators("<Control><Alt>"),
            Err(ParseError::InvalidKeysym("Empty keysym".to_string()))
        );
    }

    /// Structural regression: <Ctrl> (shorthand) must be accepted, not rejected.
    /// Users who copy configs from the C sommelier docs use <Ctrl> not <Control>.
    #[test]
    fn parse_ctrl_shorthand_is_accepted() {
        let list = parse_accelerators("<Ctrl>space").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].modifiers, CONTROL_MASK);
        assert_eq!(list[0].symbol, xkb::keysyms::KEY_space);

        // <Meta> is a synonym for <Alt>
        let list = parse_accelerators("<Meta>Tab").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].modifiers, ALT_MASK);
    }

    #[test]
    fn parse_super_modifier() {
        let list = parse_accelerators("<Super>space").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].modifiers, SUPER_MASK);
        assert_eq!(list[0].symbol, xkb::keysyms::KEY_space);
    }

    /// Regression: <Search> (ChromeOS Launcher key) must be accepted as Super.
    #[test]
    fn parse_search_alias_for_super() {
        let list = parse_accelerators("<Search>space").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].modifiers, SUPER_MASK);
        assert_eq!(list[0].symbol, xkb::keysyms::KEY_space);
    }

    /// Regression: <Win> (Windows-style key name) must be accepted as Super.
    /// Users copying configs from non-ChromeOS systems may use <Win>.
    #[test]
    fn parse_win_alias_for_super() {
        let list = parse_accelerators("<Win>space").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].modifiers, SUPER_MASK);
        assert_eq!(list[0].symbol, xkb::keysyms::KEY_space);
    }

    #[test]
    fn parse_window_placement_shortcuts_accepts_bindings() {
        let bindings =
            parse_window_placement_shortcuts("<Alt>q=top-left,<Alt>w=top,<Alt>s=fullscreen")
                .unwrap();
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].accelerator.modifiers, ALT_MASK);
        assert_eq!(bindings[0].accelerator.symbol, xkb::keysyms::KEY_q);
        assert_eq!(bindings[0].action, WindowLayoutAction::TopLeft);
        assert_eq!(bindings[2].action, WindowLayoutAction::Fullscreen);
    }

    #[test]
    fn parse_window_placement_shortcuts_rejects_duplicates_and_bad_actions() {
        assert!(parse_window_placement_shortcuts("<Alt>q=top,<Alt>q=left")
            .unwrap_err()
            .contains("duplicates"));
        assert!(parse_window_placement_shortcuts("<Alt>q=tile")
            .unwrap_err()
            .contains("unsupported action"));
        assert!(parse_window_placement_shortcuts("<Alt>q")
            .unwrap_err()
            .contains("CHORD=ACTION"));
    }
}
