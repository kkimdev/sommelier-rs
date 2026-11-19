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
    pub fn snippet(name: &str, case: &Case, choices: &[u32]) -> String {
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
        UserAction::Settle => "UserAction::Settle".to_owned(),
    }
}

/// The regressions, each replayed against the oracle.
const CASES: &[Regression] = &[Regression {
    name: "a syllable, a Backspace and a focus move, on the sequential schedule",
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
}];

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
    assert!(snippet.starts_with("Regression {\n    name: \"a syllable"));
    assert!(snippet.contains("client: ClientKind::Gtk4,"));
    assert!(snippet.contains("texts: &[\"가\", \"1\"],"));
    assert!(snippet.contains("UserAction::Key(Key::Char('r')),"));
    assert!(snippet.contains("UserAction::Focus(1),"));
    assert!(snippet.contains("choices: &[3, 0, 1],"));
}
