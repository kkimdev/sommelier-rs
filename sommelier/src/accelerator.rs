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

//! Keyboard chords: the [`Accelerator`] type and its parser.
//!
//! A chord is a keysym name optionally preceded by modifier tags, the
//! spelling used by `SOMMELIER_ACCELERATORS` and by placement bindings:
//!
//! ```text
//! Super_L,<Alt>bracketleft,<Alt>bracketright,<Control>space
//! ```
//!
//! The accepted modifier tags are listed once, in the `chord` rule of the
//! file format in `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`;
//! [`parse_accelerator`] implements that list.
//!
//! This module parses chords and computes the chord a key forms under a
//! keyboard's XKB state ([`Accelerator::of_key`]). [`HostAccelerators`]
//! is the parsed `SOMMELIER_ACCELERATORS`, which `crate::settings` reads;
//! the keyboard decides what a host accelerator means, and
//! `crate::placement_config` what a placement binding does.

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

impl Accelerator {
    /// The chord `key`, an evdev keycode, forms under the XKB state
    /// `state`: the one place a chord is computed, so host accelerators and
    /// placement bindings always match against the same value.
    ///
    /// `None` when the evdev keycode overflows the XKB offset, or when the
    /// key has no single keysym at the current level. Such a key is never
    /// an accelerator or a binding.
    pub(crate) fn of_key(state: &xkb::State, key: u32) -> Option<Self> {
        let Some(xkb_raw_keycode) = key.checked_add(8) else {
            log::warn!("Ignoring overflowing evdev keycode {}", key);
            return None;
        };
        // Use key_get_one_sym so that the full XKB state (including active shift
        // level) is considered. key_get_syms_by_level at level 0 would always
        // return the unshifted symbol, causing <Shift>-modified accelerators to
        // fail to match when Shift is held.
        //
        // key_get_one_sym returns KEY_NoSymbol when more than one keysym is
        // mapped to this key at the current shift level (e.g. dead keys or
        // Unicode combining sequences). Such a key forms no chord: we'd rather
        // forward an unknown key to the guest than accidentally suppress it.
        let sym = state.key_get_one_sym(xkb::Keycode::new(xkb_raw_keycode));
        if sym.raw() == xkb::keysyms::KEY_NoSymbol {
            return None;
        }
        Some(Self {
            modifiers: modifiers_of(state),
            symbol: keysym_to_lower(sym.raw()),
        })
    }
}

/// The modifiers `state` holds, as this module's masks: Control, Alt, Shift
/// and Super, depressed or latched.
///
/// A locked modifier does not count, as in C sommelier: an accelerator is a
/// chord of held keys. Exo reports every active modifier as depressed,
/// locks included, because its `XkbTracker` updates the mask with nothing
/// latched or locked; Caps Lock and Num Lock are not among the four, so
/// with Exo a lock never changes a chord either way.
fn modifiers_of(state: &xkb::State) -> u32 {
    let components = xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED;
    // Use the xkbcommon logical modifier name constants (e.g. MOD_NAME_ALT = "Mod1")
    // rather than raw X11 modifier group strings. These are the stable canonical names
    // that match across different keyboard layouts, matching C sommelier's use of
    // XKB_MOD_NAME_ALT, XKB_MOD_NAME_LOGO, etc.
    [
        (xkb::MOD_NAME_CTRL, CONTROL_MASK),
        (xkb::MOD_NAME_ALT, ALT_MASK),
        (xkb::MOD_NAME_SHIFT, SHIFT_MASK),
        (xkb::MOD_NAME_LOGO, SUPER_MASK),
    ]
    .into_iter()
    .filter(|&(name, _)| state.mod_name_is_active(name, components))
    .fold(0, |modifiers, (_, mask)| modifiers | mask)
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
        // self-contained; the match site (Accelerator::of_key) also
        // lowercases the incoming sym, but having both sides normalised makes the
        // invariant explicit and removes an implicit coupling.
        symbol: keysym_to_lower(sym.raw()),
    })
}

/// Parse a comma-separated chord list (the `SOMMELIER_ACCELERATORS` format).
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

/// The chords the host compositor keeps (`SOMMELIER_ACCELERATORS`): a key
/// that makes one is the host's, and no placement binding may use one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HostAccelerators(Vec<Accelerator>);

impl HostAccelerators {
    /// Parse a comma-separated chord list (see [`parse_accelerators`]).
    pub(crate) fn parse(list: &str) -> Result<Self, ParseError> {
        parse_accelerators(list).map(Self)
    }

    /// Return whether the host keeps `chord`.
    pub(crate) fn contains(&self, chord: &Accelerator) -> bool {
        self.0.contains(chord)
    }
}

impl From<Vec<Accelerator>> for HostAccelerators {
    fn from(chords: Vec<Accelerator>) -> Self {
        Self(chords)
    }
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
}
