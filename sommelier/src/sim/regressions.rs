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
