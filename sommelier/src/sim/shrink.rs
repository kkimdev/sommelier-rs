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

//! The shrinker: a failing case, made as small as it can be while it still
//! fails the same check.
//!
//! It tries three reductions, each kept only if a re-run still fails the
//! check ([`Check`]), and stops at its budget ([`Limits`]):
//! 1. ddmin over the case's actions;
//! 2. zeroing runs of the choices, longest first, from the end: choice 0 is
//!    the minimum latency or the first variant (`Choose`);
//! 3. dropping the zeros that end the choices, which a replay takes to be
//!    0 past their end anyway.
//!
//! It is deterministic: the same case and budget shrink to the same result,
//! unless the time limit cuts them off.

use std::time::{Duration, Instant};

use std::panic::{catch_unwind, AssertUnwindSafe};

use super::oracle::{Failure, Tier};
use super::regressions::Regression;
use super::sched::Choices;
use super::sched::Line;
use super::{report, run, Case, Outcome};

/// A check that failed: its tier, name and field.
pub(super) type Check = (Tier, &'static str, Option<usize>);

/// What bounds a shrink.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// Re-runs of the case.
    pub runs: usize,
    pub time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            runs: 2000,
            time: Duration::from_secs(60),
        }
    }
}

/// A shrunk case.
#[derive(Debug)]
pub(super) struct Shrunk {
    pub case: Case,
    pub choices: Vec<u32>,
    /// Re-runs spent.
    pub runs: usize,
}

/// Runs a case with some choices: `Some` with the choices it recorded if it
/// fails the check being shrunk, `None` if not.
pub(super) type Failing<'a> = dyn FnMut(&Case, &[u32]) -> Option<Vec<u32>> + 'a;

struct Shrinker<'a, 'f> {
    case: Case,
    choices: Vec<u32>,
    runs: usize,
    limits: Limits,
    start: Instant,
    failing: &'a mut Failing<'f>,
}

impl Shrinker<'_, '_> {
    fn exhausted(&self) -> bool {
        self.runs >= self.limits.runs || self.start.elapsed() >= self.limits.time
    }

    /// Keeps `case` and `choices` if they still fail.
    fn attempt(&mut self, case: Case, choices: Vec<u32>) -> bool {
        if self.exhausted() {
            return false;
        }
        self.runs += 1;
        match (self.failing)(&case, &choices) {
            Some(recorded) => {
                self.case = case;
                self.choices = recorded;
                true
            }
            None => false,
        }
    }

    /// ddmin: removes ever smaller chunks of the actions.
    fn actions(&mut self) {
        let mut parts = 2;
        while self.case.actions.len() >= 2 && !self.exhausted() {
            let len = self.case.actions.len();
            let size = len.div_ceil(parts);
            let mut start = 0;
            let mut removed = false;
            while start < self.case.actions.len() {
                let mut case = self.case.clone();
                let end = (start + size).min(case.actions.len());
                case.actions.drain(start..end);
                if self.attempt(case, self.choices.clone()) {
                    removed = true;
                } else {
                    start += size;
                }
            }
            if removed {
                parts = parts.saturating_sub(1).max(2);
            } else if size > 1 {
                parts = (parts * 2).min(len);
            } else {
                break;
            }
        }
        // One action may be left that still fails without it.
        if self.case.actions.len() == 1 {
            let mut case = self.case.clone();
            case.actions.clear();
            self.attempt(case, self.choices.clone());
        }
    }

    /// Zeroes runs of the choices, from the longest down to single ones.
    fn zero(&mut self) {
        let mut size = self.choices.len().next_power_of_two();
        while size >= 1 {
            let mut end = self.choices.len();
            // A run that draws fewer choices shortens the stream.
            while end > 0 {
                end = end.min(self.choices.len());
                if end == 0 {
                    break;
                }
                let start = end.saturating_sub(size);
                if self.choices[start..end].iter().any(|&c| c != 0) {
                    let mut choices = self.choices.clone();
                    choices[start..end].fill(0);
                    self.attempt(self.case.clone(), choices);
                }
                end = start;
            }
            size /= 2;
        }
    }

    fn trim(&mut self) {
        while self.choices.last() == Some(&0) {
            self.choices.pop();
        }
    }
}

/// Shrinks `case`, which fails with `choices`, within `limits`. `failing`
/// re-runs a candidate.
pub(super) fn shrink(
    case: &Case,
    choices: &[u32],
    limits: Limits,
    failing: &mut Failing<'_>,
) -> Shrunk {
    let mut shrinker = Shrinker {
        case: case.clone(),
        choices: choices.to_vec(),
        runs: 0,
        limits,
        start: Instant::now(),
        failing,
    };
    shrinker.actions();
    shrinker.zero();
    shrinker.trim();
    Shrunk {
        case: shrinker.case,
        choices: shrinker.choices,
        runs: shrinker.runs,
    }
}

/// Runs `case` with `choices`, its trace in `trace`; `None` if the run
/// panics, which a candidate takes as not failing the check.
fn try_run(case: &Case, choices: &[u32], trace: &mut Vec<Line>) -> Option<Outcome> {
    let run = || run(case, Choices::replay(choices.to_vec()), trace);
    catch_unwind(AssertUnwindSafe(run)).ok()
}

/// Whether `outcome` fails `check`: the one predicate that accepts a
/// candidate and confirms the result.
fn fails(outcome: &Outcome, check: Check) -> bool {
    let same = |f: &Failure| (f.tier, f.check, f.field) == check;
    outcome.failures.iter().any(same)
}

/// What a failed case shrinks to, as a report: the shrunk case's own
/// report, and the snippet that replays it. A note instead if the failure
/// does not replay from its choices.
pub(super) fn shrunk_report(case: &Case, choices: &[u32], check: Check) -> String {
    let mut failing = |case: &Case, choices: &[u32]| {
        let outcome = try_run(case, choices, &mut Vec::new())?;
        fails(&outcome, check).then_some(outcome.choices)
    };
    if failing(case, choices).is_none() {
        return "  shrinking: the failure does not replay from its choices\n".to_owned();
    }
    let shrunk = shrink(case, choices, Limits::default(), &mut failing);
    let mut trace = Vec::new();
    let outcome = try_run(&shrunk.case, &shrunk.choices, &mut trace);
    let Some(outcome) = outcome.filter(|outcome| fails(outcome, check)) else {
        return "  shrinking: the shrunk case does not fail the same check\n".to_owned();
    };
    let mut text = format!(
        "SHRUNK from {} actions and {} choices to {} and {}, in {} runs\n",
        case.actions.len(),
        choices.len(),
        shrunk.case.actions.len(),
        shrunk.choices.len(),
        shrunk.runs
    );
    text += &report(&shrunk.case, &outcome, &outcome.failures, &trace);
    text += "  paste into the CASES of sim/regressions.rs once fixed:\n";
    text += &Regression::snippet("name the bug", &shrunk.case, &shrunk.choices);
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::editor::UserAction;
    use crate::sim::ClientKind;

    /// The injected fault: a key, and later a focus move. It lives here, in
    /// the harness hook, not in the proxy.
    fn faulty(case: &Case) -> bool {
        let key = case
            .actions
            .iter()
            .position(|a| matches!(a, UserAction::Key(_)));
        let focus = case
            .actions
            .iter()
            .rposition(|a| matches!(a, UserAction::Focus(_)));
        matches!((key, focus), (Some(k), Some(f)) if k < f)
    }

    /// Re-runs for real, then adds the fault.
    fn failing(case: &Case, choices: &[u32]) -> Option<Vec<u32>> {
        let outcome = run(case, Choices::replay(choices.to_vec()), &mut Vec::new());
        faulty(case).then_some(outcome.choices)
    }

    /// A case the fault fails, with a long script and choices.
    fn big() -> Case {
        (0..)
            .map(|seed| Case::new(seed, &ClientKind::ALL))
            .find(|case| case.actions.len() >= 12 && faulty(case))
            .expect("a seed")
    }

    #[test]
    fn a_failing_case_shrinks_to_a_smaller_one_that_still_fails() {
        crate::sim::pinned(
            "sim::shrink::tests::a_failing_case_shrinks_to_a_smaller_one_that_still_fails",
            || {
                let case = big();
                let choices = run(&case, case.choices(), &mut Vec::new()).choices;
                assert!(choices.iter().any(|&c| c != 0));
                let shrunk = shrink(&case, &choices, Limits::default(), &mut failing);
                assert!(faulty(&shrunk.case));
                assert!(shrunk.case.actions.len() < case.actions.len());
                assert_eq!(shrunk.case.actions.len(), 2, "a key and a focus move");
                assert!(shrunk.choices.len() < choices.len());
                assert!(shrunk.choices.is_empty(), "every choice was free to be 0");
                assert!(failing(&shrunk.case, &shrunk.choices).is_some());
                let again = shrink(&case, &choices, Limits::default(), &mut failing);
                assert_eq!(again.case, shrunk.case, "shrinking is deterministic");
                assert_eq!(again.choices, shrunk.choices);
            },
        );
    }

    #[test]
    fn a_budget_stops_the_shrink() {
        crate::sim::pinned("sim::shrink::tests::a_budget_stops_the_shrink", || {
            let case = big();
            let choices = run(&case, case.choices(), &mut Vec::new()).choices;
            let limits = Limits {
                runs: 3,
                ..Limits::default()
            };
            let shrunk = shrink(&case, &choices, limits, &mut failing);
            assert_eq!(shrunk.runs, 3);
            assert!(faulty(&shrunk.case));
        });
    }

    #[test]
    fn a_snippet_replays_the_shrunk_case() {
        crate::sim::pinned(
            "sim::shrink::tests::a_snippet_replays_the_shrunk_case",
            || {
                let case = big();
                let mut trace = Vec::new();
                let outcome = run(&case, case.choices(), &mut trace);
                let snippet = Regression::snippet("x", &case, &outcome.choices);
                // The choices the snippet prints are the ones that replay.
                let line = snippet.lines().find(|l| l.contains("choices: &[")).unwrap();
                let list = line.split_once("&[").unwrap().1.trim_end_matches("],");
                let parsed: Vec<u32> = list
                    .split(", ")
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse().unwrap())
                    .collect();
                assert_eq!(parsed, outcome.choices);
                let mut replayed = Vec::new();
                run(&case, Choices::replay(parsed), &mut replayed);
                assert_eq!(replayed, trace);
                // One action per line, as the Rust the regression holds.
                assert_eq!(snippet.matches("UserAction::").count(), case.actions.len());
            },
        );
    }

    #[test]
    fn a_stream_that_shrinks_under_the_shrinker_is_zeroed_within_bounds() {
        // Zeroing the middle fails, and a run then draws far fewer choices.
        let case = Case::new(0, &ClientKind::ALL);
        let mut failing = |_: &Case, choices: &[u32]| {
            (choices.first() == Some(&1)).then(|| choices[..choices.len().min(2)].to_vec())
        };
        let shrunk = shrink(&case, &[1; 40], Limits::default(), &mut failing);
        assert_eq!(shrunk.choices, [1]);
    }

    #[test]
    fn a_panicking_candidate_does_not_fail_the_check() {
        let case = Case::new(0, &ClientKind::ALL);
        let mut trace = Vec::new();
        let outcome = try_run(&case, &[], &mut trace).expect("a run");
        assert!(!fails(&outcome, (Tier::Exact, "no such check", None)));
        let mut broken = case.clone();
        broken.actions = vec![UserAction::Focus(99)];
        assert!(try_run(&broken, &[], &mut trace).is_none());
    }
}
