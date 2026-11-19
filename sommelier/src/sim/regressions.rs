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

//! Regression cases of the DST: shrunk failures, pasted as [`Regression`]s.
//!
//! When a case fails, the test prints a snippet ([`Regression::snippet`])
//! of its shrunk form ([`super::shrink`]). Once the bug is fixed, paste it
//! into [`CASES`]: the case then replays on every run, against the oracle,
//! whatever the seeds do.

use super::editor::{Target, UserAction};
use super::hangul::Key;
use super::sched::Choices;
use super::{pinned, run, Case, ClientKind};

/// A case and its choices, written out so that they stay the same when the
/// generators change.
pub(super) struct Regression {
    /// What the case is about.
    pub name: &'static str,
    pub client: ClientKind,
    pub settled: bool,
    pub latin: bool,
    pub texts: &'static [&'static str],
    pub actions: &'static [UserAction],
    /// The choices that replay it ([`Choices::replay`]).
    pub choices: &'static [u32],
}

impl Regression {
    /// The case.
    fn case(&self) -> Case {
        Case {
            seed: 0,
            client: self.client,
            settled: self.settled,
            latin: self.latin,
            texts: self.texts.iter().map(|&text| text.to_owned()).collect(),
            actions: self.actions.to_vec(),
        }
    }

    /// The Rust text of a regression of `case` that replays `choices`, to
    /// paste into [`CASES`].
    pub(super) fn snippet(name: &str, case: &Case, choices: &[u32]) -> String {
        let texts: Vec<String> = case.texts.iter().map(|text| format!("{text:?}")).collect();
        let actions: Vec<String> = case.actions.iter().map(|&a| action(a)).collect();
        format!(
            "Regression {{\n    name: {name:?},\n    client: ClientKind::{:?},\n    settled: {},\n    latin: {},\n    texts: &[{}],\n    actions: &[\n        {},\n    ],\n    choices: &{choices:?},\n}},",
            case.client,
            case.settled,
            case.latin,
            texts.join(", "),
            actions.join(",\n        "),
        )
    }
}

/// The Rust text of an action.
fn action(action: UserAction) -> String {
    let target = |at: Target| match at {
        Target::Start => "Target::Start".to_owned(),
        Target::End => "Target::End".to_owned(),
        Target::BeforeMarker(n) => format!("Target::BeforeMarker({n})"),
    };
    match action {
        UserAction::Key(key) => format!("UserAction::Key(Key::{key:?})"),
        UserAction::Click { field, at } => {
            format!("UserAction::Click {{ field: {field}, at: {} }}", target(at))
        }
        UserAction::Tap { field, at } => {
            format!("UserAction::Tap {{ field: {field}, at: {} }}", target(at))
        }
        UserAction::Focus(field) => format!("UserAction::Focus({field})"),
        UserAction::KeyDown(key) => format!("UserAction::KeyDown(Key::{key:?})"),
        UserAction::KeyUp(key) => format!("UserAction::KeyUp(Key::{key:?})"),
        UserAction::RestartInputMethod => "UserAction::RestartInputMethod".to_owned(),
        UserAction::AcceptSuggestion => "UserAction::AcceptSuggestion".to_owned(),
        UserAction::Reconvert => "UserAction::Reconvert".to_owned(),
        UserAction::Reopen(field) => format!("UserAction::Reopen({field})"),
        UserAction::Settle => "UserAction::Settle".to_owned(),
    }
}

/// The regressions, each replayed against the oracle. The first is a
/// placeholder that shows the form and smoke-tests the replay: no bug
/// produced it. Real ones come from a shrunk failure.
const CASES: &[Regression] = &[
    Regression {
        name: "placeholder: a syllable, a Backspace and a focus move, on the sequential schedule",
        client: ClientKind::Gtk4,
        settled: false,
        latin: false,
        texts: &["가", "1"],
        actions: &[
            UserAction::Key(Key::Char('r')),
            UserAction::Key(Key::Char('k')),
            UserAction::Key(Key::Backspace),
            UserAction::Focus(1),
            UserAction::Settle,
        ],
        choices: &[],
    },
    Regression {
        // Seed 1000050, shrunk. The guest shows a host preedit, which Exo
        // holds before its cursor, when the input method reconverts it.
        name: "a region over a host preedit is measured from Exo's cursor",
        client: ClientKind::Chromium,
        settled: false,
        latin: false,
        texts: &["01닭2", "0닭1bb2b"],
        actions: &[
            UserAction::Key(Key::Char('e')),
            UserAction::Click {
                field: 0,
                at: Target::BeforeMarker(1),
            },
            UserAction::Key(Key::Char('z')),
            UserAction::Tap {
                field: 0,
                at: Target::BeforeMarker(2),
            },
            UserAction::Reconvert,
            UserAction::Key(Key::Char('o')),
        ],
        choices: &[
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 1291, 0, 0, 0, 0, 0, 0, 589, 0, 126, 0, 0, 0, 0, 0, 0, 0,
            1298, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 1,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 2, 0, 0, 602, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 290,
            0, 0, 0, 0, 1,
        ],
    },
Regression {
    name: "a hidden region over a shown composition, on Chromium without the extension, leaves the composition shown",
    client: ClientKind::Chromium,
    settled: false,
    latin: false,
    texts: &["0닭가1닭2", "0a12"],
    actions: &[
        UserAction::Key(Key::Char('q')),
        UserAction::KeyDown(Key::Char(' ')),
        UserAction::Key(Key::Char('f')),
        UserAction::Key(Key::Char('c')),
        UserAction::KeyUp(Key::Char(' ')),
        UserAction::Key(Key::Char('w')),
        UserAction::Click { field: 0, at: Target::Start },
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('t')),
        UserAction::Key(Key::Char('p')),
        UserAction::Settle,
        UserAction::Key(Key::Backspace),
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('b')),
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Key(Key::Char('r')),
        UserAction::Key(Key::Char('l')),
        UserAction::Key(Key::Char('h')),
        UserAction::Key(Key::Char('s')),
        UserAction::Key(Key::Char('s')),
        UserAction::Key(Key::Char('i')),
        UserAction::Key(Key::Char('l')),
        UserAction::Focus(0),
        UserAction::Tap { field: 1, at: Target::BeforeMarker(0) },
        UserAction::Click { field: 1, at: Target::BeforeMarker(0) },
        UserAction::Focus(1),
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('E')),
        UserAction::Key(Key::Char('t')),
        UserAction::Tap { field: 0, at: Target::BeforeMarker(2) },
        UserAction::Reconvert,
        UserAction::Reconvert,
    ],
    choices: &[1, 1, 0, 0, 0, 0, 0, 0, 0, 1162, 0, 2003, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 697, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 449, 0, 0, 1935, 1664, 0, 1561, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1055, 0, 0, 0, 0, 232, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 4, 2874, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1805, 0, 0, 1821, 0, 0, 785, 0, 0, 1149, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 9, 1964, 0, 3972, 1973, 0, 0, 0, 1771, 0, 0, 0, 0, 1791, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1200, 541, 0, 0, 1502, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2209, 0, 1051, 0, 0, 0, 0, 0, 0, 0, 0, 0, 705, 0, 0, 1800, 0, 0, 0, 1872, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2814, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1994, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1175, 1678, 0, 0, 0, 0, 0, 0, 482, 0, 0, 1992, 1468, 799, 2734, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2684, 0, 0, 0, 0, 0, 0, 0, 0, 0, 180, 657, 0, 0, 0, 0, 0, 0, 1858, 0, 229, 0, 0, 386, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2394, 0, 0, 0, 0, 1407, 0, 0, 0, 0, 471, 0, 1270, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2228, 0, 0, 0, 535, 0, 0, 1848, 0, 0, 0, 0, 0, 0, 0, 0, 0, 460, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 752, 0, 0, 0, 0, 791, 0, 215, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 76, 879, 489, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 746, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1554, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 109, 775, 1, 0, 503, 1938, 0, 0, 0, 0, 0, 0, 0, 160, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1993, 0, 0, 0, 0, 0, 0, 13, 0, 0, 0, 0, 491, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 714, 0, 0, 0, 0, 0, 1514, 0, 1616, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 917, 0, 1612, 0, 0, 761, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1140, 0, 0, 706, 0, 0, 0, 893, 1, 0, 1, 4, 0, 0, 0, 0, 885, 2599, 0, 0, 0, 0, 1],
},
// Seed 2452, shrunk. Exo makes a region while the guest's report, which
// leaves out the preedit Exo holds, is still at the proxy; the proxy
// sends it on while the region is on its way back (RegionCrossesReport).
Regression {
    name: "a region Exo made before a report the proxy then sent is measured in the newer report: a crossing (GTK4)",
    client: ClientKind::Gtk4,
    settled: false,
    latin: false,
    texts: &["0b닭1닭가2닭가", "0aa1a2"],
    actions: &[
        UserAction::Key(Key::Backspace),
        UserAction::Key(Key::Char('k')),
        UserAction::Focus(0),
        UserAction::Key(Key::Backspace),
        UserAction::Key(Key::Char('t')),
        UserAction::Focus(1),
        UserAction::Key(Key::Char('t')),
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('p')),
        UserAction::Reopen(1),
        UserAction::Key(Key::Char('t')),
        UserAction::Click { field: 1, at: Target::BeforeMarker(1) },
        UserAction::Click { field: 1, at: Target::BeforeMarker(1) },
        UserAction::Reconvert,
        UserAction::Reconvert,
        UserAction::Click { field: 0, at: Target::BeforeMarker(2) },
    ],
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 150, 0, 2671, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1372, 0, 0, 0, 0, 0, 0, 0, 0, 571, 0, 0, 0, 1568, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1136, 2828, 0, 0, 0, 0, 0, 0, 0, 0, 2510, 0, 0, 94, 1233, 0, 0, 1070, 0, 0, 0, 0, 2011, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 888, 0, 0, 0, 93, 0, 0, 1787, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 680, 0, 141, 1303, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1785, 0, 0, 1881, 0, 0, 0, 0, 0, 0, 0, 2645, 0, 0, 0, 0, 0, 1790, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1496, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2225, 449, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 438, 0, 0, 0, 0, 0, 0, 0, 0, 0, 755, 0, 0, 1422, 0, 0, 0, 0, 0, 641, 0, 1, 1, 674, 0, 0, 1, 4, 222, 1729, 2359, 0, 618, 988, 0, 0, 1036, 545, 485, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1520, 0, 0, 0, 0, 0, 1080, 0, 0, 1719, 0, 0, 0, 0, 0, 0, 1528, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1192, 2823, 498, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 842, 0, 2034, 1667, 0, 0, 0, 0, 0, 0, 952, 0, 0, 1, 0, 0, 0, 0, 0, 0, 826, 292, 0, 0, 0, 0, 0, 0, 0, 0, 0, 52, 0, 0, 598, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1511, 0, 0, 571, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 313, 0, 1725],
},
// Seed 8116, shrunk. Exo makes a region while the guest's report, which
// leaves out the preedit Exo holds, is still at the proxy; the proxy
// sends it on while the region is on its way back (RegionCrossesReport).
Regression {
    name: "a region Exo made before a report the proxy then sent is measured in the newer report: a crossing (GTK3)",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    texts: &["0닭닭1ba2닭b", "0가닭1a2닭b"],
    actions: &[
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('d')),
        UserAction::Key(Key::Char('x')),
        UserAction::Key(Key::Char('u')),
        UserAction::Key(Key::Char('z')),
        UserAction::Key(Key::Backspace),
        UserAction::Key(Key::Char('d')),
        UserAction::Key(Key::Backspace),
        UserAction::Tap { field: 1, at: Target::Start },
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('w')),
        UserAction::Key(Key::Backspace),
        UserAction::Key(Key::Char('j')),
        UserAction::Key(Key::Char('c')),
        UserAction::Key(Key::Char('q')),
        UserAction::Key(Key::Char('h')),
        UserAction::Key(Key::Char('R')),
        UserAction::Key(Key::Char('h')),
        UserAction::Tap { field: 0, at: Target::BeforeMarker(2) },
        UserAction::Reconvert,
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('g')),
        UserAction::Key(Key::Char('v')),
        UserAction::Key(Key::Backspace),
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('n')),
        UserAction::Key(Key::Char('v')),
        UserAction::Key(Key::Char('w')),
        UserAction::Key(Key::Char('y')),
        UserAction::Key(Key::Char('m')),
        UserAction::Key(Key::Char('b')),
        UserAction::Reopen(0),
        UserAction::Click { field: 0, at: Target::BeforeMarker(2) },
        UserAction::Tap { field: 0, at: Target::End },
        UserAction::Key(Key::Char('a')),
        UserAction::Key(Key::Char('p')),
        UserAction::Key(Key::Char('r')),
        UserAction::Tap { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Settle,
        UserAction::Tap { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Focus(0),
        UserAction::Key(Key::Char('o')),
        UserAction::Key(Key::Char('i')),
        UserAction::Reopen(1),
        UserAction::Key(Key::Char('Q')),
        UserAction::Key(Key::Char('g')),
        UserAction::Key(Key::Char('i')),
        UserAction::Key(Key::Backspace),
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Click { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Reconvert,
        UserAction::Reconvert,
        UserAction::Reconvert,
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
    ],
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1731, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1016, 0, 0, 0, 0, 0, 0, 0, 0, 1270, 0, 0, 0, 130, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1789, 1967, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 574, 678, 619, 0, 0, 0, 0, 0, 0, 0, 0, 1327, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1090, 0, 0, 0, 0, 0, 0, 0, 0, 1571, 0, 0, 0, 0, 717, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1204, 0, 106, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 675, 0, 1197, 2552, 0, 2046, 984, 0, 0, 460, 0, 0, 0, 0, 0, 0, 0, 0, 887, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 271, 0, 0, 1222, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 72, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 316, 0, 0, 0, 1095, 0, 0, 0, 0, 0, 0, 1598, 0, 0, 0, 173, 0, 0, 0, 935, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1329, 0, 0, 0, 0, 0, 0, 0, 0, 1537, 0, 1883, 0, 718, 0, 1806, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 327, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 882, 0, 0, 632, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 329, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1704, 0, 0, 0, 0, 1537, 0, 0, 0, 0, 52, 0, 0, 0, 0, 0, 0, 334, 0, 0, 0, 0, 864, 0, 1, 5, 1378, 660, 0, 0, 2853, 0, 0, 0, 952, 0, 1, 1, 0, 1223, 0, 0, 0, 0, 789, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 384, 0, 0, 0, 0, 1875, 0, 793, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 706, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 947, 0, 1796, 0, 0, 0, 534, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1997, 0, 0, 0, 0, 0, 0, 789, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 55, 213, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 407, 0, 0, 0, 1129, 954, 0, 1578, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 420, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 751, 0, 0, 0, 0, 999, 0, 1, 5, 1336, 0, 0, 0, 0, 0, 800, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 588, 0, 0, 1708, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1792, 0, 1242, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 373, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 361, 0, 0, 0, 74, 0, 0, 0, 0, 0, 1212, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1328, 516, 0, 1767, 0, 976, 0, 0, 0, 0, 0, 0, 0, 459, 0, 1717, 0, 0, 0, 0, 0, 993, 0, 9, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 854, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 447, 0, 0, 0, 0, 0, 0, 0, 0, 0, 702, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 507, 0, 0, 0, 0, 0, 0, 0, 0, 0, 309, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1568, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1328, 0, 0, 0, 1925, 0, 1, 2, 1, 0, 1, 0, 0, 0, 0, 0, 765, 2529, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 566, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 840, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 463, 726, 0, 0, 0, 0, 1728, 0, 0, 0, 0, 0, 0, 0, 118, 0, 0, 0, 0, 0, 236, 0, 0, 0, 0, 1124, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 711, 624, 886, 1146, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 718, 0, 0, 0, 0, 0, 1971, 0, 0, 0, 0, 0, 1854, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1988, 1334, 0, 0, 0, 0, 0, 0, 1695, 0, 1, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 116, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 372, 0, 0, 0, 0, 0, 0, 1257, 0, 0, 0, 0, 0, 17, 150, 0, 0, 0, 0, 0, 0, 0, 232, 0, 1, 0, 0, 0, 1, 1, 10, 0, 0, 0, 0, 0, 568, 0, 1424, 0, 0, 0, 0, 0, 0, 828, 0, 0, 0, 0, 1408, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 464, 0, 0, 0, 0, 0, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1068, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 761, 0, 438, 0, 2000, 0, 0, 0, 0, 0, 0, 0, 149, 0, 0, 0, 0, 904, 0, 59, 1325, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 413, 0, 0, 0, 0, 0, 587, 0, 0, 0, 0, 1366, 291, 1721, 1723, 0, 0, 0, 2904, 0, 0, 0, 0, 0, 0, 0, 0, 667, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1018, 0, 0, 0, 0, 0, 0, 0, 0, 409, 1894, 0, 792, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1160, 0, 0, 0, 950, 1, 4, 926, 0, 2665],
},
];

#[test]
fn every_regression_passes_the_oracle() {
    pinned(
        "sim::regressions::every_regression_passes_the_oracle",
        || {
            for regression in CASES {
                let case = regression.case();
                let choices = Choices::replay(regression.choices.to_vec());
                let outcome = run(&case, choices, &mut Vec::new());
                assert_eq!(outcome.failures, [], "regression: {}", regression.name);
            }
        },
    );
}

#[test]
fn a_snippet_names_the_case_and_its_choices() {
    let regression = &CASES[0];
    let snippet = Regression::snippet(regression.name, &regression.case(), &[3, 0, 1]);
    assert!(snippet.starts_with("Regression {\n    name: \"placeholder: a syllable"));
    assert!(snippet.contains("client: ClientKind::Gtk4,"));
    assert!(snippet.contains("texts: &[\"가\", \"1\"],"));
    assert!(snippet.contains("UserAction::Key(Key::Char('r')),"));
    assert!(snippet.contains("UserAction::Focus(1),"));
    assert!(snippet.contains("choices: &[3, 0, 1],"));
}
