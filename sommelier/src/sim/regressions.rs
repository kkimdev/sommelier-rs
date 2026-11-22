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
//! whatever the seeds do. It must still make the choices it was shrunk
//! with: a draw the models add or re-range, or a message the proxy adds or
//! drops, shifts them, so that the case replays another schedule, one that
//! may not reach its bug. And it must still reach the state its bug needs,
//! as far as the counts show ([`Regression::reaches`]).

use super::editor::{Target, UserAction};
use super::hangul::Key;
use super::oracle::Counts;
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
    pub english: bool,
    pub texts: &'static [&'static str],
    pub actions: &'static [UserAction],
    /// Whether a replay's counts show the state the bug needed: a replay
    /// that does not reach it tests something else.
    pub reaches: fn(&Counts) -> bool,
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
            english: self.english,
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
            "Regression {{\n    name: {name:?},\n    client: ClientKind::{:?},\n    settled: {},\n    latin: {},\n    english: {},\n    texts: &[{}],\n    actions: &[\n        {},\n    ],\n    reaches: |_| todo!(\"the counts of the state the bug needs\"),\n    choices: &{choices:?},\n}},",
            case.client,
            case.settled,
            case.latin,
            case.english,
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

/// The regressions, each replayed against the oracle.
const CASES: &[Regression] = &[
    Regression {
        // Seed 456, shrunk. Backspace, which the input method takes, is
        // held when Space is pressed, which it does not handle: Exo's
        // confirmation before Space (E13) comes before Space's peek (E8).
        name: "an empty confirmation before an unhandled press repeats no held key",
        client: ClientKind::Chromium,
        settled: false,
        latin: false,
        english: false,
        texts: &["0aa1b2닭닭", "0가가1닭2"],
        actions: &[
            UserAction::Key(Key::Char('v')),
            UserAction::KeyDown(Key::Backspace),
            UserAction::Key(Key::Char(' ')),
        ],
        reaches: |counts| counts.confirms_before_a_press > 0,
        choices: &[],
    },
    Regression {
        // Seed 1000050, shrunk. The guest shows a host preedit, which Exo
        // holds before its cursor, when the input method reconverts it.
        // Later draws moved its choices off that state, and the code before
        // the fix is gone, so these replay its actions on a schedule that
        // reaches it again (`Regression::reaches`), found by search and
        // shrunk.
        name: "a region over a host preedit is measured from Exo's cursor",
        client: ClientKind::Chromium,
        settled: false,
        latin: false,
        english: false,
        texts: &["01닭2", "0닭1bb2b"],
        actions: &[
            UserAction::Tap {
                field: 0,
                at: Target::BeforeMarker(2),
            },
            UserAction::Reconvert,
            UserAction::Key(Key::Char('o')),
        ],
        reaches: |counts| {
            counts.exo.regions_over_compositions > 0 && counts.regions_over_preedits > 0
        },
        choices: &[
            0, 0, 0, 0, 0, 1973, 0, 0, 0, 0, 770, 2906, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2108, 0,
            0, 0, 0, 0, 0, 0, 0, 399, 0, 0, 0, 1864, 0, 0, 0, 0, 2000, 0, 0, 0, 0, 0, 0, 1915, 0, 0,
            0, 0, 0, 0, 0, 0, 526, 0, 0, 0, 0, 0, 0, 0, 0, 502, 0, 0, 0, 824, 0, 761, 0, 0, 0, 0, 0,
            0, 0, 2308, 0, 0, 1,
        ],
    },
// Seed 2452, shrunk. Exo makes a region before it gets a guest report,
// which leaves out the preedit Exo holds, that the proxy has sent: the
// region crosses the report on its way back, and the proxy measures it in
// Exo's text before the report
// (`row_X1_a_region_made_before_a_report_is_measured_before_it`).
// Later draws moved its choices off that crossing, and the code before
// the fix is gone, so these replay its actions on a schedule that crosses
// again (`Regression::reaches`), found by search, with runs of choices
// zeroed.
Regression {
    name: "a region Exo made before a report the proxy had sent is measured before the report (GTK4)",
    client: ClientKind::Gtk4,
    settled: false,
    latin: false,
    english: false,
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
    reaches: |counts| counts.regions_crossing_reports > 0,
    choices: &[2, 1, 0, 0, 0, 0, 0, 0, 0, 805, 0, 1274, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 824, 0, 0, 0, 0, 0, 0, 0, 0, 963, 0, 0, 0, 880, 2648, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1298, 0, 0, 0, 0, 0, 0, 0, 439, 0, 0, 0, 0, 0, 0, 2322, 0, 0, 117, 0, 1, 0, 0, 0, 1, 19, 0, 5422, 0, 112, 0, 36504, 0, 0, 0, 2937, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2835, 0, 0, 34938, 0, 0, 1204, 0, 0, 0, 34185, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 836, 2804, 0, 1145, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 249, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 44, 0, 1058, 0, 0, 0, 0, 0, 0, 0, 0, 1, 12, 0, 15010, 2702, 0, 0, 0, 0, 1, 7, 1986, 0, 2620, 0, 0, 0, 1433, 0, 0, 0, 0, 0, 953, 0, 0, 0, 0, 1209, 0, 0, 1522, 0, 0, 0, 0, 0, 2042, 0, 0, 0, 0, 0, 2641, 0, 0, 0, 0, 0, 1, 5, 0, 0, 0, 0, 1, 8, 132, 0, 34422, 0, 1890, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 166, 1806, 0, 1391, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 1278, 0, 0, 0, 0, 0, 514, 0, 0, 2412, 0, 0, 0, 0, 0, 0, 0, 0, 149, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 850, 2953, 126, 0, 0, 0, 25264, 0, 0, 15819, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1268, 1837, 0, 1522, 0, 0, 0, 0, 0, 0, 0, 0, 2894, 0, 0, 10606, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 677, 2813, 0, 1991, 0, 0, 0, 0, 0, 0, 33703, 0, 0, 0, 1, 10, 0, 0, 0, 0, 0, 154, 0, 0, 1988, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2464, 0, 0, 1495, 0, 0, 0, 0, 0, 0, 0, 263, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 427, 2488, 0, 89, 0, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 199, 38, 2489, 433, 0, 0, 1679, 0, 0, 0, 1493, 0, 0, 0, 0, 917, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2807, 1910, 117, 2707, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2145, 0, 0, 141, 0, 0, 0, 0, 0, 1831, 0, 0, 0, 1, 17, 0, 27922, 0, 1, 19, 726, 1014, 0, 2633, 0, 0, 0, 0, 0, 0, 0, 750, 0, 0, 0, 1682, 0, 0, 0, 1992, 0, 0, 0, 0, 1496, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1955, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1599, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1260, 2599],
},
// Seed 8116, shrunk. Exo makes a region before it gets a guest report,
// which leaves out the preedit Exo holds, that the proxy has sent: the
// region crosses the report on its way back, and the proxy measures it in
// Exo's text before the report
// (`row_X1_a_region_made_before_a_report_is_measured_before_it`).
// Later draws moved its choices off that crossing, and the code before
// the fix is gone, so these replay its actions on a schedule that crosses
// again (`Regression::reaches`), found by search, with runs of choices
// zeroed.
Regression {
    name: "a region Exo made before a report the proxy had sent is measured before the report (GTK3)",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    english: false,
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
    reaches: |counts| counts.regions_crossing_reports > 0,
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 535, 0, 1505, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2891, 0, 0, 0, 0, 0, 0, 0, 0, 2711, 0, 0, 2690, 0, 2898, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2292, 0, 0, 1860, 0, 2483, 0, 0, 0, 0, 0, 0, 0, 336, 2106, 162, 0, 0, 658, 0, 1587, 0, 0, 0, 0, 0, 0, 0, 0, 972, 0, 0, 0, 64, 0, 0, 1816, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1435, 0, 0, 0, 0, 0, 1170, 0, 0, 0, 0, 2020, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1525, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 730, 0, 262, 0, 14, 0, 1, 0, 0, 0, 0, 0, 0, 656, 1461, 2502, 0, 0, 2868, 274, 0, 672, 1032, 0, 0, 788, 0, 0, 2210, 0, 0, 11, 0, 0, 0, 0, 0, 1850, 2654, 847, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1147, 0, 105, 1494, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2400, 0, 58, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 939, 0, 992, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1469, 0, 0, 0, 0, 0, 0, 0, 779, 0, 0, 0, 0, 1523, 0, 0, 0, 0, 0, 0, 0, 909, 347, 0, 1930, 0, 0, 0, 0, 0, 928, 0, 1899, 0, 0, 0, 0, 0, 0, 2266, 2961, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1879, 0, 0, 0, 0, 1, 6, 598, 1521, 0, 0, 1508, 0, 1176, 0, 0, 0, 0, 1256, 0, 0, 0, 0, 0, 0, 2246, 0, 0, 0, 445, 0, 1, 9, 1416, 1687, 0, 422, 0, 0, 2954, 0, 0, 78, 1058, 0, 0, 0, 0, 1945, 735, 0, 0, 0, 0, 115, 0, 0, 18, 1156, 2565, 0, 421, 653, 0, 0, 0, 0, 0, 0, 0, 279, 0, 0, 0, 1451, 1739, 0, 0, 0, 0, 0, 0, 0, 0, 2242, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1851, 0, 402, 2528, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1735, 0, 0, 0, 0, 699, 0, 0, 1630, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2421, 0, 0, 0, 0, 0, 0, 0, 0, 1832, 0, 0, 0, 0, 1882, 0, 0, 0, 0, 1303, 0, 0, 0, 0, 0, 0, 271, 0, 0, 0, 0, 2003, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1126, 1045, 0, 0, 0, 0, 0, 0, 0, 0, 308, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1931, 0, 0, 0, 0, 0, 1106, 0, 0, 0, 1169, 0, 0, 0, 0, 2917, 0, 0, 0, 0, 2289, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2263, 386, 1382, 0, 0, 1786, 0, 0, 0, 0, 0, 0, 1501, 0, 0, 0, 0, 0, 0, 363, 0, 895, 1634, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2454, 0, 0, 1867, 0, 0, 0, 0, 0, 0, 1898, 0, 0, 0, 0, 789, 1895, 0, 0, 0, 0, 0, 0, 0, 2892, 0, 1970, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 241, 1637, 0, 0, 0, 0, 0, 0, 0, 0, 0, 389, 0, 0, 0, 0, 0, 0, 0, 0, 1126, 0, 0, 0, 0, 0, 0, 19, 0, 0, 19, 0, 108, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2247, 2557, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1608, 0, 0, 550, 0, 0, 0, 0, 0, 24, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2414, 0, 0, 83, 0, 0, 0, 0, 2643, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1259, 0, 0, 0, 0, 0, 0, 0, 0, 0, 55, 0, 1434, 0, 0, 292, 0, 0, 0, 0, 0, 0, 0, 1348, 409, 0, 0, 0, 1412, 1254, 0, 0, 0, 0, 0, 0, 0, 402, 0, 2316, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 931, 0, 2615, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 36, 0, 0, 0, 1991, 2036, 0, 0, 0, 0, 2475, 0, 0, 0, 0, 933, 106, 1839, 0, 0, 0, 0, 0, 1378, 0, 0, 0, 0, 2364, 916, 0, 2162, 740, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1961, 0, 0, 0, 0, 2407, 796, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 139, 1092, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2492, 0, 0, 0, 0, 727, 0, 0, 1779, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2766, 0, 695, 0, 1, 5, 1043, 814, 1329, 0, 1337, 0, 0, 1807, 0, 0, 0, 0, 0, 847, 0, 1, 10, 1674, 1838, 0, 0, 0, 0, 0, 0, 515, 2272, 0, 0, 0, 0, 0, 2939, 0, 0, 0, 0, 0, 365, 0, 1559, 0, 0, 0, 0, 2624, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 545, 0, 0, 0, 0, 516, 0, 0, 337, 0, 0, 0, 0, 0, 0, 0, 2114, 0, 1321, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2341, 0, 0, 0, 1633, 0, 0, 0, 0, 1313, 0, 0, 0, 0, 0, 0, 0, 0, 0, 740, 0, 1561, 0, 0, 0, 0, 1, 2, 1565, 2790, 0, 0, 0, 0, 0, 109, 1850, 0, 0, 2482, 0, 0, 0, 0, 0, 0, 0, 1901, 1076, 1743, 0, 0, 0, 0, 0, 1508, 0, 1023, 1974, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1452, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1383, 0, 0, 0, 0, 1415, 0, 0, 0, 0, 70, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1756, 0, 0, 0, 0, 0, 0, 0, 0, 821, 0, 272, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1045, 0, 0, 0, 0, 0, 0, 0, 1036, 0, 291, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1544, 0, 0, 0, 0, 0, 2751, 0, 2199, 2697, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 222, 1031, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1867, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2535, 0, 0, 0, 0, 0, 0, 2832, 0, 0, 0, 0, 747, 0, 0, 0, 0, 1942, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 471, 1256, 1801, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1020, 0, 302, 0, 1460, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1632, 0, 0, 0, 0, 0, 1806, 0, 0, 0, 0, 679, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1136, 0, 240, 1709, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 921, 368, 1379, 0, 1877, 489, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2870, 0, 0, 1889, 0, 0, 0, 0, 0, 0, 0, 0, 2312, 2552, 0, 0, 0, 0, 0, 392, 0, 0, 0, 0, 0, 1694, 1372, 0, 0, 0, 0, 0, 0, 0, 2680, 0, 0, 0, 0, 0, 342, 0, 1, 6, 400, 864, 0, 0, 2250, 0, 0, 0, 222, 1741, 0, 0, 0, 1036, 0, 0, 0, 1705, 0, 0, 670, 0, 1, 2, 1274, 2335, 0, 0, 0, 0, 0, 0, 0, 0, 908, 522, 1752, 0, 0, 518, 0, 0, 0, 950, 487, 723, 0, 0, 0, 1298, 0, 1580, 0, 0, 0, 2882, 1026, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1465, 2638, 484, 1904, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 867, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2327, 0, 2849, 0, 0, 0, 0, 0, 0, 0, 0, 360, 0, 0, 0, 0, 0, 0, 0, 0, 617, 0, 0, 0, 0, 0, 0, 0, 0, 292, 1001, 1294, 0, 0, 2920, 0, 0, 0, 0, 1, 4, 2689, 0, 0, 0, 1641, 0, 955, 0, 0, 1069, 0, 0, 0, 1695, 0, 0, 2529, 0, 0, 1707, 0, 0, 0, 0, 0, 1, 3, 0, 0, 0, 0, 0, 0, 954, 0, 0, 0, 0, 0, 0, 2603],
},
// Seed 18858, unshrunk by the panic; shrunk on a stand-in check. The guest
// reopens the window the keyboard focus is moving to, so the proxy drops
// Exo's enter, then passes on the key Exo sent with that focus.
// Later draws moved its choices off the state its bug needs, and the code
// before the fix is gone, so these replay its actions on a schedule
// that reaches that state again (`Regression::reaches`), found by
// search, with runs of choices zeroed.
Regression {
    name: "keys Exo sent to a surface the guest destroyed, after an enter the proxy dropped, reach no window",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    english: false,
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
    reaches: |counts| counts.keys_to_no_window > 0,
    choices: &[1, 0, 0, 0, 0, 0, 0, 2707, 2742, 1354, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1674, 0, 0, 0, 0, 0, 0, 0, 0, 253, 0, 0, 0, 598, 1435, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2657, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 611, 1453, 0, 0, 0, 0, 0, 0, 0, 2844, 0, 1, 4, 0, 2054, 1936, 0, 0, 0, 0, 1846, 0, 0, 1, 3, 984, 1922, 0, 0, 0, 0, 0, 2149, 0, 0, 2370, 0, 0, 0, 0, 0, 0, 1818, 505, 0, 1, 4, 12833, 131, 0, 1368, 0, 0, 0, 0, 0, 0, 1187, 0, 0, 0, 0, 768, 0, 1893, 0, 1, 18, 0, 0, 0, 1736, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2249, 0, 0, 0, 0, 0, 0, 0, 2967, 0, 0, 961, 12585, 2645, 0, 0, 0, 1851, 0, 1874, 0, 0, 0, 0, 0, 1, 16, 881, 1808, 0, 0, 0, 0, 0, 3284, 0, 0, 0, 2833, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 799, 1540, 0, 0, 0, 2568, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1189, 0, 1717, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2020, 0, 1587, 2794, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1055, 0, 0, 0, 0, 0, 2415, 0, 0, 0, 1244, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 3, 5372, 709, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1],
},
// Seed 50166, shrunk. GTK3 moves its cursor on a click without a report
// (C7, C8), so Exo's region, made at the old cursor, shows at the new one;
// the report that follows drops it (E4) and moves Exo's cursor, so Exo
// refuses the engine's next, grown region, which lies past the text.
// Later draws moved its choices off the state its bug needs, and the code
// before the fix is gone, so these replay its actions on a schedule
// that reaches that state again (`Regression::reaches`), found by
// search, with runs of choices zeroed.
Regression {
    name: "a reconversion Exo refused, past the text a GTK3 click moved, leaves the engine the region it had",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    english: false,
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
    reaches: |counts| counts.regions_refused > 0,
    choices: &[2, 1, 0, 0, 0, 0, 0, 0, 0, 1136, 0, 1492, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2549, 0, 0, 0, 0, 0, 0, 0, 0, 811, 0, 0, 0, 1660, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 158, 2857, 0, 0, 0, 0, 0, 0, 0, 1977, 0, 0, 0, 0, 0, 0, 1401, 0, 0, 263, 0, 1, 10, 19, 0, 0, 0, 39, 0, 0, 0, 14, 0, 0, 0, 171, 388, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 466, 740, 0, 0, 0, 0, 0, 0, 21445, 0, 0, 0, 0, 104, 0, 0, 2569, 0, 0, 0, 0, 398, 0, 0, 1863, 0, 0, 0, 0, 0, 0, 0, 0, 2768, 0, 0, 0, 0, 957, 0, 0, 13251, 0, 556, 0, 3173, 0, 0, 0, 0, 0, 1655, 0, 0, 0, 1744, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 862, 0, 0, 38375, 0, 0, 0, 851, 0, 20993, 0, 2973, 0, 0, 0, 0, 0, 0, 97, 692, 0, 0, 0, 0, 0, 582, 0, 0, 0, 0, 0, 613, 0, 0, 984, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1240, 2096, 0, 789, 0, 2469, 0, 0, 0, 0, 0, 0, 0, 43008, 0, 923, 0, 21672, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1213, 0, 244, 0, 2265, 0, 0, 1, 0, 0, 0, 0, 0, 838, 2601, 0, 1094, 0, 0, 0, 0, 0, 0, 1537, 0, 0, 0, 0, 0, 0, 820, 0, 0, 2285, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2907, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2189, 2210, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1406, 0, 0, 213, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1549, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1042, 0, 0, 0, 0, 0, 0, 2407, 0, 0, 0, 0, 0, 2058, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 148, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1714, 0, 346, 0, 0, 0, 0, 0, 0, 943, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2683, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2744, 0, 0, 0, 0, 0, 0, 0, 0, 0, 301, 2679, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2856, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 279, 0, 0, 0, 0, 0, 0, 0, 441, 0, 0, 0, 42481, 0, 0, 40439, 0, 0, 0, 0, 131, 0, 0, 40683, 0, 0, 0, 0, 1079, 0, 0, 34853, 0, 0, 0, 0, 549, 0, 0, 26807, 0, 0, 0, 0, 1657, 0, 0, 0, 0, 0, 0, 1822, 0, 0, 0, 0, 2540, 0, 0, 0, 0, 0, 0, 0, 1025, 0, 854, 0, 1, 15, 0, 7913, 0, 1, 2, 0, 0, 1, 10, 812, 0, 15633, 0, 0, 1840, 0, 648, 0, 0, 1, 3, 0, 1728, 0, 0, 0, 2362, 0, 0, 0, 0, 0, 0, 0, 128, 2723, 0, 0, 43614, 0, 0, 26014, 0, 0, 1467, 0, 0, 0, 3510, 0, 0, 0, 0, 180, 0, 0, 0, 0, 827, 0, 0, 0, 0, 0, 0, 2577, 0, 0, 0, 0, 1739, 0, 0, 17274, 0, 0, 0, 1, 12, 1376, 0, 2011, 0, 2595, 0, 0, 0, 1, 13, 2743, 0, 818, 0, 0, 0, 0, 0, 0, 0, 1381, 113, 0, 0, 0, 1142, 0, 0, 1384, 0, 0, 0, 1987, 0, 2527, 0, 0, 1171, 0, 0, 0, 0, 0, 0, 0, 1913, 0, 0, 0, 0, 2956, 0, 0, 1797, 0, 0, 0, 0, 0, 927, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12674, 0, 2692, 856, 0, 0, 873, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 359, 711, 0, 0, 0, 0, 0, 19, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 15, 579, 1777, 1606, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 866, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2612, 2821, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1650, 0, 0, 0, 0, 0, 0, 226, 0, 0, 977, 0, 0, 0, 0, 0, 0, 0, 87, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 214, 2622, 0, 743, 1911, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1595, 0, 1584, 0, 0, 0, 0, 1876, 0, 0, 0, 0, 1463, 0, 0, 0, 2682, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28565, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1342, 0, 2687, 0, 0, 27939, 0, 0, 0, 16562, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1861, 0, 455, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1133, 0, 0, 0, 0, 2207, 0, 0, 31279, 0, 0, 0, 31378, 0, 0, 0, 0, 771, 0, 0, 0, 0, 1532, 0, 0, 0, 0, 0, 0, 1002, 1448, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1495, 0, 172, 0, 0, 0, 0, 0, 439, 0, 0, 753, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2897, 0, 0, 1872, 0, 0, 0, 0, 1892, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1777, 2273, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 984, 0, 0, 0, 1],
},
// Seed 12364, shrunk. An engine answer made before a click moves the
// focus shows a preedit in the newly focused field, which Exo drops at
// GTK3's next report (E4). The proxy drops the first reconversion's
// region, measured with that preedit, which it splits, and Exo composes
// from it, so the next region is measured over a preedit Exo no longer
// holds. The proxy now drops what it cannot measure with the guest out of
// step with Exo (`Drops::out_of_step`).
// Later draws moved its choices off the state its bug needs, and the code
// before the fix is gone, so these replay its actions on a schedule
// that reaches that state again (`Regression::reaches`), found by
// search, with runs of choices zeroed.
Regression {
    name: "a region the proxy drops, measured with a late preedit Exo dropped (E4), explains the next region",
    client: ClientKind::Gtk3,
    settled: false,
    latin: false,
    english: false,
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
    reaches: |counts| counts.drops.out_of_step > 0 && counts.regions > 0,
    choices: &[1, 1, 0, 0, 0, 0, 0, 0, 0, 1853, 0, 2444, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2024, 0, 0, 0, 0, 0, 0, 0, 0, 1283, 0, 0, 0, 668, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 918, 1455, 1910, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 247, 0, 0, 0, 0, 0, 0, 1902, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 431, 2905, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1733, 799, 0, 0, 0, 0, 0, 0, 0, 1831, 0, 0, 898, 959, 0, 0, 0, 0, 0, 0, 0, 1233, 2244, 0, 0, 0, 0, 36, 0, 1272, 0, 0, 0, 0, 0, 0, 0, 0, 77, 0, 0, 0, 0, 0, 0, 2351, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2660, 0, 0, 0, 0, 0, 1, 1, 0, 0, 414, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 844, 0, 1737, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1906, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2007, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 523, 872, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1918, 0, 1259, 0, 0, 292, 0, 737, 0, 1141, 0, 0, 0, 577, 0, 0, 0, 0, 0, 0, 124, 0, 2761, 0, 0, 0, 0, 0, 0, 0, 793, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1894, 378, 217, 0, 0, 0, 0, 1, 15, 8877, 5418, 0, 0, 2997, 0, 1423, 0, 0, 0, 1, 3, 1953, 0, 0, 0, 804, 0, 0, 0, 0, 0, 1051, 1895, 2828, 0, 109, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2756, 0, 0, 0, 0, 1160, 0, 0, 0, 0, 2774, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 5, 2975, 0, 996, 1057, 0, 0, 0, 0, 0, 1165, 0, 0, 0, 0, 1960, 0, 0, 0, 198, 0, 1631, 0, 1622, 0, 0, 0, 0, 0, 0, 1775, 0, 0, 0, 0, 498, 0, 2364, 0, 0, 0, 0, 0, 0, 1933, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1022, 2709, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 156, 1040, 0, 1695, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2201, 0, 2907, 0, 0, 0, 1, 8, 1469, 0, 0, 0, 1934, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 542, 2449, 0, 866, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 362, 0, 0, 0, 0, 0, 0, 0, 1275, 0, 0, 0, 0, 0, 0, 0, 2097, 2672, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 50, 0, 672, 0, 0, 1786, 2963, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2555, 1953, 0, 0, 0, 0, 1734, 0, 0, 0, 0, 0, 0, 223, 1345, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1655, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 946, 2928, 0, 0, 0, 0, 0, 2912, 0, 0, 0, 0, 0, 0, 1802, 0, 0, 0, 0, 0, 0, 1472, 441, 0, 0, 0, 0, 0, 1],
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
    english: false,
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
    reaches: |counts| counts.reports_withheld > 0,
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
    english: false,
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
    reaches: |counts| counts.exo.stale_presses > 0,
    choices: &[2, 0, 0, 0, 0, 0, 0, 2654, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1797, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1441, 0, 0, 0, 0, 0, 0, 0, 227, 1587, 1932, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 931, 1480, 0, 0, 1916, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 19, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 766, 0, 0, 0, 0, 0, 0, 205, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 261, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1304, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
},
Regression {
    name: "a confirmation before the unannounced press of a key forwarded before a restart recovers nothing",
    client: ClientKind::Chromium,
    settled: false,
    latin: false,
    english: false,
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
    reaches: |counts| counts.exo.stale_presses_forwarded > 0,
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
    english: false,
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
    reaches: |counts| counts.exo.stale_keys_forgotten > 0,
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
    english: false,
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
    reaches: |counts| counts.exo.held_regions_confirmed > 0,
    choices: &[2, 1, 0, 0, 0, 0, 0, 0, 0, 1275, 0, 2926, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 424, 0, 0, 0, 0, 0, 0, 0, 0, 2992, 0, 0, 0, 0, 1, 0, 1224, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 15, 0, 0, 1297, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 934, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 148, 1073, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1546, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 19],
},
// Seed 30618, shrunk. Two suggestions in a row: Exo's delete and commit of
// the first reach Chromium in two transactions under one serial, so the
// report after the delete closes C1's gate and Chromium holds the text
// after the commit, Exo's, which the proxy measures the second delete in.
// The model's check accepted only the texts it had sent, and so took that
// delete for a protocol violation.
Regression {
    name: "a delete may count in the text Chromium holds behind its gate",
    client: ClientKind::Chromium,
    settled: false,
    latin: false,
    english: false,
    texts: &["0가1b가2", "01b2"],
    actions: &[
        UserAction::Key(Key::Char('l')),
        UserAction::Click { field: 0, at: Target::End },
        UserAction::Key(Key::Char('g')),
        UserAction::Key(Key::Char('e')),
        UserAction::Key(Key::Char('j')),
        UserAction::Click { field: 1, at: Target::BeforeMarker(0) },
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('W')),
        UserAction::Tap { field: 1, at: Target::Start },
        UserAction::Key(Key::Char('v')),
        UserAction::Tap { field: 0, at: Target::End },
        UserAction::Click { field: 1, at: Target::End },
        UserAction::Key(Key::Char('p')),
        UserAction::Key(Key::Char('r')),
        UserAction::Click { field: 0, at: Target::BeforeMarker(1) },
        UserAction::Key(Key::Char('d')),
        UserAction::RestartInputMethod,
        UserAction::AcceptSuggestion,
        UserAction::AcceptSuggestion,
    ],
    reaches: |counts| counts.deletes > 1 && counts.crossed_reports > 0,
    choices: &[1, 0, 0, 0, 0, 0, 0, 0, 0, 588, 0, 2413, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1709, 0, 0, 0, 0, 0, 0, 0, 481, 0, 0, 0, 368, 2849, 0, 1991, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1742, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 187, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2518, 0, 0, 1671, 686, 0, 1167, 0, 0, 0, 0, 0, 0, 292, 0, 1, 4, 0, 333, 0, 479, 0, 0, 1824, 952, 0, 0, 447, 0, 1, 0, 0, 0, 0, 0, 0, 1980, 0, 0, 0, 0, 0, 1444, 0, 363, 720, 1261, 0, 0, 0, 0, 0, 1830, 0, 0, 0, 0, 1153, 0, 1869, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1756, 2349, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 423, 43, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1084, 0, 1839, 0, 1232, 891, 0, 0, 0, 0, 1098, 0, 0, 0, 0, 0, 621, 0, 1, 11, 2835, 645, 0, 2707, 0, 0, 0, 0, 0, 0, 0, 0, 1902, 0, 0, 1807, 0, 0, 0, 0, 1033, 0, 0, 80, 1387, 0, 0, 0, 0, 0, 0, 0, 0, 1750, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1874, 0, 0, 0, 2930, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1328, 0, 0, 0, 0, 0, 0, 881, 0, 0, 0, 0, 0, 259, 0, 0, 0, 0, 0, 0, 0, 1088, 0, 0, 0, 0, 707, 1521, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2366, 0, 0, 0, 935, 1378, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2528, 262, 0, 1821, 0, 1807, 0, 0, 0, 0, 0, 0, 2540, 2718, 0, 186, 1139, 0, 0, 0, 0, 0, 0, 2518, 0, 0, 0, 0, 0, 1718, 0, 0, 0, 2305, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1663, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1971, 635, 0, 0, 0, 1880, 0, 0, 0, 355, 1494, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2516, 0, 0, 0, 0, 0, 0, 0, 0, 1, 8, 2051, 0, 1472, 1962, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1213, 0, 0, 0, 0, 0, 1869, 0, 0, 0, 0, 2594, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 461, 0, 1222, 0, 575, 0, 2507, 0, 1, 2, 2377, 0, 0, 0, 0, 0, 0, 0, 0, 733, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1443, 1588, 0, 0, 0, 0, 1869, 966, 1, 0, 0, 0, 0, 0, 434, 2208, 0, 0, 1357],
},
// Seed 1026063, shrunk; settled. Exo composes z (ㅋ) before it gets the
// report of Space, so it resets to the report and drops its composition
// (E4), while the guest shows ㅋ. The restart's confirmation commits ㅋ in
// the guest alone, and the guest's report then resets Exo to its text with
// ㅋ. The
// confirmation came after the guest's commit before that report, so the
// proxy took the guest to be out of step and dropped the reconversion's
// region over ㅋ, which then doubled at the next key.
Regression {
    name: "a report of a confirmed preedit Exo no longer held puts the guest in step",
    client: ClientKind::Chromium,
    settled: true,
    latin: false,
    english: false,
    texts: &["0닭닭1가b2가", "01aa2가"],
    actions: &[
        UserAction::Key(Key::Char(' ')),
        UserAction::Key(Key::Char('z')),
        UserAction::Settle,
        UserAction::RestartInputMethod,
        UserAction::Settle,
        UserAction::Reconvert,
    ],
    reaches: |counts| counts.host_detaches > 0 && counts.regions > 0,
    choices: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 961, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 31, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1501, 2020, 0, 0, 724, 0, 1, 0, 0, 0, 0, 0, 0, 0, 538],
},
];

#[test]
fn every_regression_passes_the_oracle() {
    pinned(
        "sim::regressions::every_regression_passes_the_oracle",
        || {
            let failed: Vec<String> = CASES
                .iter()
                .filter_map(|regression| {
                    let problems = problems(regression);
                    let name = regression.name;
                    (!problems.is_empty())
                        .then(|| format!("{name:?}:\n  {}", problems.join("\n  ")))
                })
                .collect();
            assert!(
                failed.is_empty(),
                "regressions failed:\n{}",
                failed.join("\n")
            );
        },
    );
}

/// What is wrong with `regression`'s replay, if anything: the oracle's
/// failures; or else choices other than those it stores, from a change to
/// what the models or the proxy draw; or else, on its stored schedule, no
/// longer the state its bug needs, from a change to what the proxy or the
/// models do there.
fn problems(regression: &Regression) -> Vec<String> {
    let case = regression.case();
    let choices = Choices::replay(regression.choices.to_vec());
    let outcome = run(&case, choices, &mut Vec::new());
    let mut problems = Vec::new();
    if !outcome.failures.is_empty() {
        problems.push(format!("it fails the oracle: {:?}", outcome.failures));
    }
    let pairs = outcome.choices.iter().zip(regression.choices);
    let drawn = pairs.take_while(|(drawn, stored)| drawn == stored).count();
    if drawn < regression.choices.len() {
        problems.push(format!(
            "its replay draws other choices from choice {drawn} on, so it replays another \
             schedule: the models or the proxy draw differently now. If the proxy changed, \
             rule out a regression, then shrink the case again"
        ));
    } else if !(regression.reaches)(&outcome.counts) {
        problems.push(
            "on its own schedule it no longer reaches the state its bug needs: the proxy or \
             the models behave differently there now. Find out why, as a regression of the \
             proxy shows here, before shrinking it again"
                .to_owned(),
        );
    }
    problems
}

#[test]
fn a_snippet_names_the_case_and_its_choices() {
    let regression = Regression {
        name: "a syllable and a focus move",
        client: ClientKind::Gtk4,
        settled: false,
        latin: false,
        english: false,
        texts: &["가", "1"],
        actions: &[UserAction::Key(Key::Char('r')), UserAction::Focus(1)],
        reaches: |_| true,
        choices: &[],
    };
    let snippet = Regression::snippet(regression.name, &regression.case(), &[3, 0, 1]);
    assert!(snippet.starts_with("Regression {\n    name: \"a syllable and a focus move\","));
    assert!(snippet.contains("client: ClientKind::Gtk4,"));
    assert!(snippet.contains("texts: &[\"가\", \"1\"],"));
    assert!(snippet.contains("UserAction::Key(Key::Char('r')),"));
    assert!(snippet.contains("UserAction::Focus(1),"));
    assert!(snippet.contains("reaches: |_| todo!("));
    assert!(snippet.contains("choices: &[3, 0, 1],"));
}
