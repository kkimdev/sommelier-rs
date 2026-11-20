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
        // Seed 456, shrunk. Backspace, which the input method takes, is
        // held when Space is pressed, which it does not handle: Exo's
        // confirmation before Space (E13) comes before Space's peek (E8).
        name: "an empty confirmation before an unhandled press repeats no held key",
        client: ClientKind::Chromium,
        settled: false,
        latin: false,
        texts: &["0aa1b2닭닭", "0가가1닭2"],
        actions: &[
            UserAction::Key(Key::Char('v')),
            UserAction::KeyDown(Key::Backspace),
            UserAction::Key(Key::Char(' ')),
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
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 1291, 0, 0, 0, 0, 0, 0, 589, 0, 126, 0, 0, 0, 0, 0, 0, 0,
            1298, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 1,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 2, 0, 0, 602, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 290,
            0, 0, 0, 0, 1,
        ],
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
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 150, 0, 2671, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1372, 0, 0, 0, 0, 0, 0, 0, 0, 571, 0, 0, 0, 1568, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1136, 2828, 0, 0, 0, 0, 0, 0, 0, 0, 2510, 0, 0, 94, 1233, 0, 0, 1070, 0, 0, 0, 0, 2011, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 888, 0, 0, 0, 93, 0, 0, 1787, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 680, 0, 141, 1303, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1785, 0, 0, 1881, 0, 0, 0, 0, 0, 0, 0, 2645, 0, 0, 0, 0, 0, 1790, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1496, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2225, 449, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 438, 0, 0, 0, 0, 0, 0, 0, 0, 0, 755, 0, 0, 1422, 0, 0, 0, 0, 0, 641, 0, 1, 1, 674, 0, 0, 1, 4, 222, 1729, 2359, 0, 618, 988, 0, 0, 1036, 545, 485, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1520, 0, 0, 0, 0, 0, 1080, 0, 0, 1719, 0, 0, 0, 0, 0, 0, 1528, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1192, 2823, 498, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 842, 0, 2034, 1667, 0, 0, 0, 0, 0, 0, 952, 0, 0, 1, 0, 0, 0, 0, 0, 0, 826, 292, 0, 0, 0, 0, 0, 0, 0, 0, 0, 52, 0, 0, 598, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1511, 0, 0, 571, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 313, 0, 1725],
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
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1731, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1016, 0, 0, 0, 0, 0, 0, 0, 0, 1270, 0, 0, 0, 130, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1789, 1967, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 574, 678, 619, 0, 0, 0, 0, 0, 0, 0, 0, 1327, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1090, 0, 0, 0, 0, 0, 0, 0, 0, 1571, 0, 0, 0, 0, 717, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1204, 0, 106, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 675, 0, 1197, 2552, 0, 2046, 984, 0, 0, 460, 0, 0, 0, 0, 0, 0, 0, 0, 887, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 271, 0, 0, 1222, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 72, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 316, 0, 0, 0, 1095, 0, 0, 0, 0, 0, 0, 1598, 0, 0, 0, 173, 0, 0, 0, 935, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1329, 0, 0, 0, 0, 0, 0, 0, 0, 1537, 0, 1883, 0, 718, 0, 1806, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 327, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 882, 0, 0, 632, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 329, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1704, 0, 0, 0, 0, 1537, 0, 0, 0, 0, 52, 0, 0, 0, 0, 0, 0, 334, 0, 0, 0, 0, 864, 0, 1, 5, 1378, 660, 0, 0, 2853, 0, 0, 0, 952, 0, 1, 1, 0, 1223, 0, 0, 0, 0, 789, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 384, 0, 0, 0, 0, 1875, 0, 793, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 706, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 947, 0, 1796, 0, 0, 0, 534, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1997, 0, 0, 0, 0, 0, 0, 789, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 55, 213, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 407, 0, 0, 0, 1129, 954, 0, 1578, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 420, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 751, 0, 0, 0, 0, 999, 0, 1, 5, 1336, 0, 0, 0, 0, 0, 800, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 588, 0, 0, 1708, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1792, 0, 1242, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 373, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 361, 0, 0, 0, 74, 0, 0, 0, 0, 0, 1212, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1328, 516, 0, 1767, 0, 976, 0, 0, 0, 0, 0, 0, 0, 459, 0, 1717, 0, 0, 0, 0, 0, 993, 0, 9, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 854, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 447, 0, 0, 0, 0, 0, 0, 0, 0, 0, 702, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 507, 0, 0, 0, 0, 0, 0, 0, 0, 0, 309, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1568, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1328, 0, 0, 0, 1925, 0, 1, 2, 1, 0, 1, 0, 0, 0, 0, 0, 765, 2529, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 566, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 840, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 463, 726, 0, 0, 0, 0, 1728, 0, 0, 0, 0, 0, 0, 0, 118, 0, 0, 0, 0, 0, 236, 0, 0, 0, 0, 1124, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 711, 624, 886, 1146, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 718, 0, 0, 0, 0, 0, 1971, 0, 0, 0, 0, 0, 1854, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1988, 1334, 0, 0, 0, 0, 0, 0, 1695, 0, 1, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 116, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 372, 0, 0, 0, 0, 0, 0, 1257, 0, 0, 0, 0, 0, 17, 150, 0, 0, 0, 0, 0, 0, 0, 232, 0, 1, 0, 0, 0, 1, 1, 10, 0, 0, 0, 0, 0, 568, 0, 1424, 0, 0, 0, 0, 0, 0, 828, 0, 0, 0, 0, 1408, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 464, 0, 0, 0, 0, 0, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1068, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 761, 0, 438, 0, 2000, 0, 0, 0, 0, 0, 0, 0, 149, 0, 0, 0, 0, 904, 0, 59, 1325, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 413, 0, 0, 0, 0, 0, 587, 0, 0, 0, 0, 1366, 291, 1721, 1723, 0, 0, 0, 2904, 0, 0, 0, 0, 0, 0, 0, 0, 667, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1018, 0, 0, 0, 0, 0, 0, 0, 0, 409, 1894, 0, 792, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1160, 0, 0, 0, 950, 1, 4, 926, 0, 2665],
},
// Seed 18858, unshrunk by the panic; shrunk on a stand-in check. The guest
// reopens the window the keyboard focus is moving to, so the proxy drops
// Exo's enter, then passes on the key Exo sent with that focus.
Regression {
    name: "keys Exo sent to a surface the guest destroyed, after an enter the proxy dropped, reach no window",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    texts: &["0닭1가2닭", "0a1a닭2b"],
    actions: &[
        UserAction::Key(Key::Char('j')),
        UserAction::Click { field: 0, at: Target::BeforeMarker(0) },
        UserAction::RestartInputMethod,
        UserAction::Key(Key::Char('y')),
        UserAction::Focus(1),
        UserAction::Key(Key::Char('g')),
        UserAction::Click { field: 0, at: Target::Start },
        UserAction::Settle,
        UserAction::Key(Key::Char(' ')),
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Key(Key::Char('n')),
        UserAction::Reopen(1),
    ],
    choices: &[2, 1, 0, 0, 0, 0, 0, 0, 1536, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 785, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 225, 570, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 69, 0, 0, 0, 1, 20, 0, 1874, 823, 0, 0, 557, 0, 0, 0, 1867, 2221, 0, 0, 1, 2, 1566, 0, 0, 0, 0, 0, 0, 2746, 1978, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1734, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1036, 1, 231, 0, 0, 2152, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 770, 0, 0, 0, 0, 0, 989, 1239, 0, 0, 0, 0, 1, 0, 2735, 0, 0, 0, 0, 0, 0, 0, 1617, 1976, 0, 0, 0, 0, 0, 0, 1051, 0, 796, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1960, 0, 0, 0, 0, 0, 0, 1793, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1939, 410, 990, 0, 0, 1910, 0, 0, 0, 0, 0, 590, 0, 154, 0, 0, 0, 0, 1636, 555, 0, 0, 0, 0, 0, 0, 1834, 0, 0, 0, 0, 0, 0, 1426, 0, 1874, 0, 0, 0, 0, 0, 246, 1035, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 291, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2461, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1],
},
// Seed 50166, shrunk. GTK3 moves its cursor on a click without a report
// (C7, C8), so Exo's region, made at the old cursor, shows at the new one;
// the report that follows drops it (E4) and moves Exo's cursor, so Exo
// refuses the engine's next, grown region, which lies past the text.
Regression {
    name: "a reconversion Exo refused, past the text a GTK3 click moved, leaves the engine the region it had",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    texts: &["0a1닭2닭", "0닭b1가2"],
    actions: &[
        UserAction::Key(Key::Char('l')),
        UserAction::Key(Key::Char('l')),
        UserAction::Key(Key::Char('P')),
        UserAction::Key(Key::Char('e')),
        UserAction::Key(Key::Char('u')),
        UserAction::Click { field: 0, at: Target::BeforeMarker(2) },
        UserAction::Click { field: 0, at: Target::BeforeMarker(0) },
        UserAction::Key(Key::Char('g')),
        UserAction::Focus(1),
        UserAction::RestartInputMethod,
        UserAction::Key(Key::Char('x')),
        UserAction::Key(Key::Backspace),
        UserAction::KeyDown(Key::Char(' ')),
        UserAction::Key(Key::Char('v')),
        UserAction::KeyUp(Key::Char(' ')),
        UserAction::Key(Key::Char('z')),
        UserAction::Key(Key::Char('z')),
        UserAction::Key(Key::Char('i')),
        UserAction::Key(Key::Backspace),
        UserAction::Click { field: 0, at: Target::BeforeMarker(0) },
        UserAction::Key(Key::Char('x')),
        UserAction::Key(Key::Char('j')),
        UserAction::Key(Key::Char('h')),
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('a')),
        UserAction::Tap { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Click { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Reconvert,
        UserAction::Reconvert,
        UserAction::Click { field: 0, at: Target::Start },
    ],
    choices: &[2, 1, 0, 0, 0, 0, 0, 852, 1771, 1397, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 553, 0, 0, 0, 0, 1344, 1643, 0, 0, 0, 0, 0, 0, 1872, 2487, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1306, 0, 0, 0, 0, 0, 13, 0, 0, 523, 0, 1, 1, 0, 0, 0, 0, 1, 17, 0, 0, 0, 656, 0, 0, 0, 0, 1883, 0, 0, 0, 0, 0, 0, 1, 4, 1, 0, 0, 0, 0, 0, 0, 1, 15, 0, 0, 0, 0, 0, 0, 234, 0, 0, 0, 0, 0, 1179, 0, 0, 0, 1962, 0, 0, 330, 0, 0, 0, 0, 0, 0, 1761, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1468, 1187, 0, 0, 0, 0, 0, 0, 0, 0, 0, 935, 0, 906, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 673, 0, 0, 0, 0, 1231, 0, 0, 0, 0, 0, 0, 2984, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1303, 128, 0, 0, 0, 0, 0, 0, 474, 0, 0, 1811, 0, 0, 0, 0, 0, 0, 2561, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 682, 0, 0, 0, 0, 0, 145, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1501, 0, 0, 0, 0, 0, 753, 1696, 0, 0, 0, 0, 0, 0, 0, 0, 335, 1427, 1178, 0, 0, 0, 0, 1846, 0, 0, 147, 1345, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 231, 969, 0, 0, 0, 0, 1274, 0, 0, 0, 0, 0, 0, 140, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 146, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 723, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 15, 340, 0, 0, 0, 0, 0, 2, 832, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 310, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 340, 0, 0, 0, 0, 668, 0, 0, 0, 0, 0, 0, 961, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1047, 0, 0, 0, 973, 1027, 0, 0, 0, 0, 0, 0, 0, 0, 1749, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 358, 0, 0, 0, 0, 681, 0, 0, 0, 0, 0, 39048, 0, 0, 0, 0, 0, 0, 0, 2199, 0, 0, 159, 1901, 1985, 1922, 0, 0, 0, 0, 0, 0, 727, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1816, 0, 0, 0, 2698, 0, 0, 0, 0, 245, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1065, 0, 0, 558, 0, 1, 4, 0, 1794, 0, 0, 1, 19, 665, 0, 455, 0, 2718, 0, 915, 0, 0, 1555, 0, 0, 0, 0, 0, 1, 19, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 18, 71, 0, 77, 1634, 1922, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1952, 0, 101, 0, 0, 0, 0, 1449, 1856, 0, 0, 0, 0, 1825, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 385, 438, 600, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 595, 0, 0, 0, 0, 0, 0, 1, 4, 0, 1211, 2698, 0, 0, 0, 0, 0, 0, 113, 1798, 0, 0, 57, 0, 0, 0, 0, 0, 0, 0, 0, 357, 0, 0, 0, 0, 0, 0, 1374, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 407, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 261, 0, 0, 1928, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 655, 0, 1, 0, 0, 473, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 532, 1, 0, 0, 1, 11, 0, 0, 0, 1],
},
// Seed 12364, shrunk. An engine answer made before a click moves the
// focus shows a preedit in the newly focused field, which Exo drops at
// GTK3's next report (E4). The proxy drops the first reconversion's
// region, measured with that preedit, which it splits, and Exo composes
// from it, so the next region is measured over a preedit Exo no longer
// holds (`RegionOverUnheldPreedit`).
Regression {
    name: "a region the proxy drops, measured with a late preedit Exo dropped (E4), explains the next region",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    texts: &["01닭b2", "01닭2a"],
    actions: &[
        UserAction::Key(Key::Char('p')),
        UserAction::Key(Key::Char('m')),
        UserAction::Tap { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Key(Key::Char('g')),
        UserAction::KeyDown(Key::Backspace),
        UserAction::KeyUp(Key::Backspace),
        UserAction::Key(Key::Char('t')),
        UserAction::Key(Key::Char('u')),
        UserAction::Settle,
        UserAction::Reopen(0),
        UserAction::Settle,
        UserAction::Key(Key::Char(' ')),
        UserAction::Click { field: 0, at: Target::End },
        UserAction::RestartInputMethod,
        UserAction::Tap { field: 1, at: Target::BeforeMarker(0) },
        UserAction::Key(Key::Char('h')),
        UserAction::Key(Key::Char('q')),
        UserAction::Tap { field: 1, at: Target::BeforeMarker(1) },
        UserAction::Key(Key::Char('W')),
        UserAction::Key(Key::Char('u')),
        UserAction::Click { field: 0, at: Target::BeforeMarker(2) },
        UserAction::Reconvert,
        UserAction::Reconvert,
    ],
    choices: &[1, 0, 0, 0, 0, 0, 0, 2991, 0, 714, 0, 0, 280, 2019, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2393, 0, 0, 0, 0, 0, 0, 0, 0, 2886, 0, 0, 0, 1799, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 243, 1417, 0, 0, 0, 0, 0, 0, 299, 0, 1, 17, 8632, 122, 807, 0, 0, 2655, 1575, 0, 0, 0, 0, 0, 0, 1823, 969, 0, 1, 12, 847, 0, 0, 0, 0, 8556, 818, 2785, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2547, 1392, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 1468, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 186, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2903, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1178, 2573, 0, 0, 0, 0, 1008, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 390, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1614, 1080, 626, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1701, 1085, 0, 0, 0, 0, 0, 0, 0, 0, 2202, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1230, 1859, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1948, 2072, 0, 2536, 0, 0, 0, 0, 0, 0, 0, 2157, 0, 0, 162, 1, 0, 0, 0, 1004, 0, 0, 0, 0, 0, 0, 0, 0, 214, 1628, 0, 0, 0, 0, 0, 0, 1642, 0, 0, 0, 0, 0, 0, 858, 1280, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 933, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 234, 0, 1797, 0, 0, 0, 0, 0, 0, 0, 1659, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 705, 0, 1122, 0, 0, 0, 0, 0, 0, 791, 0, 0, 645, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2115, 0, 0, 798, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 1, 6, 0, 2182],
},
// Seed 4372, shrunk. A reconversion makes a region of committed text the
// preedit, and the user moves the focus at once: GTK4's report without the
// preedit, which the proxy withholds, reaches it after Exo moved the focus
// and detached the input method. Exo's tracker still holds the report with
// that composition (E4), so the withholding is right; the oracle looked
// only at a text input with an input method attached.
Regression {
    name: "a report withheld after Exo moved the focus is held to the detached text input's tracker",
    client: ClientKind::Gtk4,
    settled: false,
    latin: false,
    texts: &["0닭1a2b", "0닭닭12닭닭"],
    actions: &[
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Key(Key::Char('f')),
        UserAction::Key(Key::Char('m')),
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Reopen(0),
        UserAction::Reconvert,
        UserAction::Focus(0),
    ],
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 1224, 0, 1751, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2722, 0, 0, 0, 0, 0, 0, 0, 0, 1571, 0, 0, 0, 963, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1763, 1871, 2812, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 446, 0, 90, 357, 0, 1668, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2075, 0, 0, 0, 322, 1173, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 902, 0, 0, 0, 0, 0, 1213, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1432, 0, 0, 1588, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 719, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1762, 0, 0, 1150, 0, 0, 0, 0, 0, 1302, 0, 806, 0, 1, 8, 1799, 0, 0, 0, 0, 2064, 0, 1882, 0, 0, 0, 1912, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10, 271, 1119, 0, 0, 0, 0, 1125, 0, 0, 0, 0, 0, 0, 0, 1481, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1914, 0, 0, 1862, 0, 0, 0, 0, 0, 0, 0, 0, 641, 0, 0, 0, 0, 0, 331, 969, 1743, 1, 0, 0, 1, 3, 1685, 0, 0, 0, 0, 0, 0, 295, 830, 0, 0, 1877],
},
// Seeds 13104 and 16710, shrunk. Space's release dies with the input
// method as it restarts, so Exo's keyboard still holds Space, and Space
// pressed again while Backspace is held gets no peek, only the input
// method's empty confirmation (E13), which the proxy took for Backspace's
// autorepeat. In 13104 the input method took Space; in 16710 it reached
// the guest.
Regression {
    name: "a confirmation while a key held across a restart is held recovers nothing",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    texts: &["0가12", "01닭2가"],
    actions: &[
        UserAction::Tap { field: 1, at: Target::BeforeMarker(1) },
        UserAction::Key(Key::Char('a')),
        UserAction::Key(Key::Char('w')),
        UserAction::Key(Key::Char('e')),
        UserAction::Key(Key::Char('m')),
        UserAction::Key(Key::Char('w')),
        UserAction::Key(Key::Char('x')),
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('s')),
        UserAction::RestartInputMethod,
        UserAction::KeyDown(Key::Backspace),
        UserAction::Key(Key::Char(' ')),
        UserAction::KeyUp(Key::Backspace),
    ],
    choices: &[2, 0, 0, 0, 0, 0, 0, 2654, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1797, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1441, 0, 0, 0, 0, 0, 0, 0, 227, 1587, 1932, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 931, 1480, 0, 0, 1916, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 19, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 766, 0, 0, 0, 0, 0, 0, 205, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 261, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1304, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
},
Regression {
    name: "a confirmation before the unannounced press of a key forwarded before a restart recovers nothing",
    client: ClientKind::Chromium,
    settled: false,
    latin: false,
    texts: &["0닭1닭2가a", "0가a12닭b"],
    actions: &[
        UserAction::Click { field: 1, at: Target::BeforeMarker(2) },
        UserAction::Click { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::KeyDown(Key::Char(' ')),
        UserAction::KeyUp(Key::Char(' ')),
        UserAction::Key(Key::Char('c')),
        UserAction::RestartInputMethod,
        UserAction::KeyDown(Key::Backspace),
        UserAction::Key(Key::Char(' ')),
        UserAction::KeyUp(Key::Backspace),
    ],
    choices: &[2, 1, 0, 0, 0, 0, 0, 2310, 2829, 1770, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1598, 1830, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 886, 1521, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 755, 0, 0, 0, 0, 0, 0, 2, 1197, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1717, 0, 0, 0, 0, 913, 0, 0, 0, 0, 0, 0, 1131, 0, 0, 1477, 0, 0, 450, 0, 0, 0, 0, 0, 1430, 1619, 0, 0, 0, 0, 0, 0, 0, 1, 4, 0, 1435, 1943, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2315, 0, 118, 0, 1703, 0, 0, 1831, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1682, 1, 1041, 0, 0, 0, 1931, 0, 0, 0, 0, 2000, 0, 0, 1615, 0, 529, 1233, 0, 0, 0, 1408, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 435, 0, 0, 0, 0, 0, 918, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1042, 0, 0, 0, 0, 1, 9, 0, 0, 0, 0, 1],
},
// Seed 16710, shrunk again once the proxy recovered nothing while Space
// was held across the restart. A click moves the focus before Space,
// pressed again, reaches Exo's keyboard, which takes the keys it holds
// from the seat as it enters the new surface (`Keyboard::SetFocus`), so
// it announces that press with a peek: the model kept Space held, sent
// only the confirmation, and the proxy, whose keyboard the leave cleared,
// recovered Backspace for it.
Regression {
    name: "a keyboard enter forgets a key whose release died with the input method",
    client: ClientKind::Chromium,
    settled: false,
    latin: false,
    texts: &["0닭1닭2가a", "0가a12닭b"],
    actions: &[
        UserAction::Click { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('w')),
        UserAction::Click { field: 0, at: Target::BeforeMarker(0) },
        UserAction::KeyDown(Key::Char(' ')),
        UserAction::KeyUp(Key::Char(' ')),
        UserAction::Key(Key::Char('c')),
        UserAction::RestartInputMethod,
        UserAction::KeyDown(Key::Backspace),
        UserAction::Key(Key::Char(' ')),
        UserAction::KeyUp(Key::Backspace),
        UserAction::Click { field: 1, at: Target::BeforeMarker(1) },
    ],
    choices: &[2, 1, 0, 0, 0, 0, 0, 2310, 2829, 1770, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2070, 0, 0, 0, 0, 2899, 0, 0, 0, 0, 0, 0, 0, 1154, 0, 0, 0, 0, 1280, 0, 0, 0, 0, 0, 0, 2254, 0, 0, 0, 0, 0, 0, 0, 0, 535, 0, 0, 0, 0, 0, 0, 0, 652, 0, 0, 0, 0, 0, 0, 0, 2063, 0, 0, 0, 0, 0, 0, 1061, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1809, 0, 0, 317, 0, 0, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0, 193, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 853, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 776, 0, 0, 0, 0, 0, 1963, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 862, 0, 0, 0, 0, 2919, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 614, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 624, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 122, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 19, 0, 0, 0, 0, 0, 0, 1],
},
// Seed 14452, shrunk. A reconversion's region is held in IMA for the
// Backspace in flight, which the engine commits over and does not handle.
// IMA's confirmation before passing Backspace on composes the held region
// and confirms it (`ConfirmComposition` returns early only with a commit
// alone); the model returned early, then sent the region after the
// commit, so Exo composed a region it never confirmed, which the guest
// showed while Backspace edited around it.
Regression {
    name: "a confirmation before an unhandled key composes the region IMA holds for it",
    client: ClientKind::Chromium,
    settled: false,
    latin: false,
    texts: &["012ba", "0가a1가2b닭"],
    actions: &[
        UserAction::Key(Key::Char('e')),
        UserAction::Click { field: 0, at: Target::BeforeMarker(0) },
        UserAction::Click { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Click { field: 1, at: Target::BeforeMarker(1) },
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('k')),
        UserAction::Key(Key::Char('h')),
        UserAction::Focus(1),
        UserAction::Key(Key::Char('d')),
        UserAction::RestartInputMethod,
        UserAction::Reconvert,
        UserAction::Key(Key::Backspace),
    ],
    choices: &[2, 1, 0, 0, 0, 0, 0, 0, 0, 1275, 0, 2926, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 424, 0, 0, 0, 0, 0, 0, 0, 0, 2992, 0, 0, 0, 0, 1, 0, 1224, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 15, 0, 0, 1297, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 934, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 148, 1073, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1546, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 19],
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
