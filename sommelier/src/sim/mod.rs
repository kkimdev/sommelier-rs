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

//! The deterministic simulation test (DST) of the IME bridge.
//!
//! A case types into a simulated client ([`client`]) through the real
//! proxy, driven by a [`proxy::harness::Session`], with the Exo model
//! ([`exo`]), its asynchronous engine ([`engine`]) and the host's display
//! ([`display`]) behind it. Every message travels a FIFO channel with a
//! drawn latency on one virtual clock ([`sched`]), so the only reorderings
//! are those the real system allows, and a seed fixes everything, timing
//! included. The user ([`user`]) types Korean or Latin, clicks, taps and
//! moves the focus. Two oracles judge the case once it settles
//! ([`oracle`]): tier 1, every case, holds the proxy to the specification
//! of `docs/IME_REDESIGN.md`, section 2, whatever the timing; tier 2,
//! settled cases only, holds the client to the user's intent. The design's
//! section 4 describes it whole.
//!
//! - `SOMMELIER_DST_SEED` (0) and `SOMMELIER_DST_CASES` (600) choose the
//!   seeds. An even seed is a free case, which tier 1 judges in
//!   [`ime_reaches_the_specified_text`]; an odd one is settled, which both
//!   tiers judge in [`ime_reaches_the_intended_text`].
//! - `SOMMELIER_DST_CLIENT=chromium|gtk3|gtk4` restricts the client
//!   models, and `SOMMELIER_DST_MODE=free|settled` the modes.
//!
//! [`proxy::harness::Session`]: crate::proxy::harness::Session

mod client;
mod clock;
mod display;
mod editor;
mod engine;
pub(crate) mod exo;
mod golden;
mod hangul;
mod keymap;
pub(crate) mod oracle;
mod sched;
mod user;

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::test_support::Rng;
use editor::{Reference, TextModel, Typing, UserAction};
use engine::Lag;
use oracle::{Counts, Failure, KnownIssue, Tier};
use sched::{Choices, Line, Setup, Tally, World};

/// A client model a case types into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientKind {
    Chromium,
    Gtk3,
    Gtk4,
}

impl ClientKind {
    const ALL: [Self; 3] = [Self::Chromium, Self::Gtk3, Self::Gtk4];

    fn name(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Gtk3 => "gtk3",
            Self::Gtk4 => "gtk4",
        }
    }
}

/// What a case does. Its choices come from its seed too.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Case {
    seed: u64,
    client: ClientKind,
    /// A settled case, which the user-intent oracle judges too.
    settled: bool,
    /// The input method is a Latin layout rather than Korean.
    latin: bool,
    /// The fields' initial texts.
    texts: Vec<String>,
    actions: Vec<UserAction>,
}

impl Case {
    /// The case of `seed`, settled if the seed is odd, typing into one of
    /// `clients`: a Latin layout one case in four.
    fn new(seed: u64, clients: &[ClientKind]) -> Self {
        let mut rng = Rng::new(seed);
        let client = *rng.pick(clients);
        let latin = rng.one_in(4);
        let settled = seed % 2 == 1;
        let texts = user::texts(&mut rng);
        let mut case = Self {
            seed,
            client,
            settled,
            latin,
            texts,
            actions: Vec::new(),
        };
        case.actions = user::actions(&mut rng, &case.texts, case.typing(), settled);
        case
    }

    /// How keys reach the client's fields: GTK's Backspace leaves a
    /// syllable's NFD prefix (C14).
    fn typing(&self) -> Typing {
        Typing {
            latin: self.latin,
            nfd_backspace: self.client != ClientKind::Chromium,
        }
    }

    /// The choices the seed draws.
    fn choices(&self) -> Choices {
        Choices::new(self.seed ^ 0x5eed_5eed)
    }
}

/// What a case came to.
#[derive(Debug)]
struct Outcome {
    /// The checks that failed, unless a known issue explains them.
    failures: Vec<Failure>,
    /// The known issue that explains the failures, if one does.
    known: Option<KnownIssue>,
    counts: Counts,
    tally: Tally,
    /// The text-input extension the host had, and the engine's lag.
    extension: Option<u32>,
    lag: Lag,
    /// The choices the case made, which replay it ([`Choices::replay`]).
    choices: Vec<u32>,
}

/// Runs `case` with `choices`, its trace in `trace`.
fn run(case: &Case, mut choices: Choices, trace: &mut Vec<Line>) -> Outcome {
    let extension = exo::draw_extension_version(&mut choices);
    let lag = Lag::draw(&mut choices);
    let setup = Setup {
        client: case.client,
        latin: case.latin,
        extension,
        lag,
        texts: &case.texts,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let mut world = World::new(&setup, choices, trace).await;
        world.run(&case.actions).await;
        let mut outcome = Outcome {
            failures: Vec::new(),
            known: None,
            counts: Counts::default(),
            tally: world.tally,
            extension,
            lag,
            choices: world.choices.recorded().to_vec(),
        };
        let (Some(spec), Some(shadow), false) = (&world.spec, &world.shadow, world.ended) else {
            outcome.failures.push(Failure {
                tier: Tier::Exact,
                check: "session",
                field: None,
                detail: "the proxy ended the session".to_owned(),
            });
            return outcome;
        };
        outcome.counts = spec.counts;
        let engine = world.engine_composition();
        let mut failures = oracle::exact(spec, shadow, &world.exo, &engine, extension);
        if case.settled {
            let fields = case.texts.iter().map(|text| TextModel::new(text)).collect();
            let mut reference = Reference::new(fields).typing(case.typing());
            for &action in &case.actions {
                reference.apply(action);
            }
            failures.extend(oracle::intended(&reference, world.client()));
        }
        outcome.known = oracle::classify(&failures, &world.left_composing);
        if outcome.known.is_none() {
            outcome.failures = failures;
        }
        outcome
    })
}

/// The report of a failed case, with the end of its trace.
fn report(case: &Case, outcome: &Outcome, failures: &[Failure], trace: &[Line]) -> String {
    let mode = if case.settled { "settled" } else { "free" };
    let mut report = format!(
        "DST FAILURE seed={} mode={mode} client={} latin={} ext={:?} lag={:?}\n",
        case.seed,
        case.client.name(),
        case.latin,
        outcome.extension,
        outcome.lag,
    );
    for failure in failures {
        let tier = match failure.tier {
            Tier::Exact => "tier1",
            Tier::Intent => "tier2",
        };
        report += &format!(
            "  check: {tier} {}, field {:?}\n",
            failure.check, failure.field
        );
        for line in failure.detail.lines() {
            report += &format!("    {line}\n");
        }
    }
    let shown = env_u64("SOMMELIER_DST_TRACE_LINES", 150) as usize;
    let skipped = trace.len().saturating_sub(shown);
    report += &format!("  trace (t in ms; {skipped} earlier lines not shown):\n");
    for line in &trace[skipped..] {
        report += &format!("    {line}\n");
    }
    report += &format!("  actions: {:?}\n", case.actions);
    report += &format!(
        "  replay: SOMMELIER_DST_SEED={} SOMMELIER_DST_CASES=1 cargo test -p sommelier --bins sim::\n",
        case.seed
    );
    report
}

/// The value of environment variable `name`, or `default`.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} is not a number"))
    })
}

/// The cases of the run the environment asks for, of one mode.
fn cases(settled: bool) -> Vec<Case> {
    let mode = std::env::var("SOMMELIER_DST_MODE").unwrap_or_default();
    if !mode.is_empty() && mode != if settled { "settled" } else { "free" } {
        return Vec::new();
    }
    let clients: Vec<ClientKind> = match std::env::var("SOMMELIER_DST_CLIENT") {
        Ok(name) => {
            let client = ClientKind::ALL.into_iter().find(|kind| kind.name() == name);
            vec![client.unwrap_or_else(|| panic!("no client model {name}"))]
        }
        Err(_) => ClientKind::ALL.to_vec(),
    };
    let first = env_u64("SOMMELIER_DST_SEED", 0);
    let count = env_u64("SOMMELIER_DST_CASES", 600);
    (first..first + count)
        .filter(|seed| (seed % 2 == 1) == settled)
        .map(|seed| Case::new(seed, &clients))
        .collect()
}

/// What a run of cases came to.
#[derive(Debug, Default)]
struct Summary {
    cases: u64,
    /// The cases a known issue explains, with the issue.
    known: Vec<(u64, KnownIssue)>,
    counts: Counts,
    tally: Tally,
    /// The reports of the cases that failed.
    failed: Vec<String>,
}

/// Runs `cases`, each in a fresh world; one in 50 runs twice, and its two
/// traces must be identical.
fn run_all(cases: &[Case]) -> Summary {
    let mut summary = Summary::default();
    for case in cases {
        let mut trace = Vec::new();
        let outcome = catch_unwind(AssertUnwindSafe(|| run(case, case.choices(), &mut trace)));
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(panic) => {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                let failure = Failure {
                    tier: Tier::Exact,
                    check: "panic",
                    field: None,
                    detail: message,
                };
                let outcome = Outcome {
                    failures: Vec::new(),
                    known: None,
                    counts: Counts::default(),
                    tally: Tally::default(),
                    extension: None,
                    lag: Lag::Fast,
                    choices: Vec::new(),
                };
                summary
                    .failed
                    .push(report(case, &outcome, &[failure], &trace));
                continue;
            }
        };
        if case.seed % 50 == 0 {
            let mut again = Vec::new();
            run(case, case.choices(), &mut again);
            if again != trace {
                let first = trace.iter().zip(&again).position(|(a, b)| a != b);
                let detail = format!("the traces first differ at line {first:?}");
                let failure = Failure {
                    tier: Tier::Exact,
                    check: "determinism",
                    field: None,
                    detail,
                };
                summary
                    .failed
                    .push(report(case, &outcome, &[failure], &trace));
            }
        }
        summary.cases += 1;
        summary
            .known
            .extend(outcome.known.map(|issue| (case.seed, issue)));
        let (counts, tally) = (&mut summary.counts, &mut summary.tally);
        counts.press_commits += outcome.counts.press_commits;
        counts.fence_drops += outcome.counts.fence_drops;
        counts.late_applied += outcome.counts.late_applied;
        counts.raw_keys += outcome.counts.raw_keys;
        tally.late += outcome.tally.late;
        tally.dropped += outcome.tally.dropped;
        if !outcome.failures.is_empty() {
            summary
                .failed
                .push(report(case, &outcome, &outcome.failures, &trace));
        }
    }
    summary
}

/// Runs `body`, the body of test `test`, in a process of its own whose
/// XKB defaults are pinned, so that a seed replays the same on any
/// machine; prints what it printed.
fn pinned(test: &str, body: impl FnOnce()) {
    let xkb = [
        ("XKB_DEFAULT_RULES", "evdev"),
        ("XKB_DEFAULT_MODEL", "pc105"),
        ("XKB_DEFAULT_LAYOUT", "us"),
        ("XKB_DEFAULT_VARIANT", ""),
        ("XKB_DEFAULT_OPTIONS", ""),
    ];
    print!(
        "{}",
        crate::test_support::in_own_process_with_env(test, &xkb, body)
    );
}

/// Runs the cases of one mode ([`pinned`]), and fails with the report of
/// the first case that failed. A run of the default size or more must also
/// have reached what the mode is there to reach: in free cases, presses
/// that end a composition, fence drops, late answers the guest got and
/// raw keys; in settled ones, presses and the known issue.
fn check(test: &str, settled: bool) {
    pinned(test, || {
        let summary = run_all(&cases(settled));
        let Summary {
            cases,
            known,
            counts,
            tally,
            failed,
        } = &summary;
        println!("{cases} cases, {counts:?}, {tally:?}");
        let mut issues: Vec<KnownIssue> = known.iter().map(|&(_, issue)| issue).collect();
        issues.sort();
        issues.dedup();
        for issue in issues {
            let seeds: Vec<u64> = known.iter().filter(|k| k.1 == issue).map(|k| k.0).collect();
            println!(
                "known issue {issue:?} in {} cases, seeds {seeds:?}: {}; tracked in {}",
                seeds.len(),
                issue.reason(),
                issue.tracking()
            );
        }
        let reached = match settled {
            false => vec![
                counts.press_commits,
                counts.fence_drops,
                counts.late_applied,
                counts.raw_keys,
            ],
            true => vec![counts.press_commits, known.len() as u64],
        };
        if *cases >= 300 {
            let known = known.len();
            assert!(
                !reached.contains(&0),
                "a counter stayed at 0: {counts:?}, {known} known"
            );
        }
        for report in failed {
            let heading = report.lines().filter(|line| !line.starts_with("    "));
            heading
                .take_while(|line| !line.starts_with("  trace"))
                .for_each(|line| println!("{line}"));
        }
        if let Some(first) = failed.first() {
            panic!(
                "{} of {cases} DST cases failed; the first:\n{first}",
                failed.len()
            );
        }
    });
}

#[test]
fn ime_reaches_the_specified_text() {
    check("sim::ime_reaches_the_specified_text", false);
}

#[test]
fn ime_reaches_the_intended_text() {
    check("sim::ime_reaches_the_intended_text", true);
}

#[test]
fn a_case_replays_from_its_recorded_choices() {
    let case = Case::new(4, &ClientKind::ALL);
    let mut trace = Vec::new();
    let outcome = run(&case, case.choices(), &mut trace);
    assert!(trace.len() > 100 && outcome.failures.is_empty());
    let mut replayed = Vec::new();
    let again = run(
        &case,
        Choices::replay(outcome.choices.clone()),
        &mut replayed,
    );
    assert_eq!(replayed, trace);
    assert_eq!(again.choices, outcome.choices);
    // No choices at all is the most sequential schedule, a case too.
    let sequential = run(&case, Choices::replay(Vec::new()), &mut Vec::new());
    assert!(sequential.choices.iter().all(|&choice| choice == 0));
    assert_eq!(sequential.failures, []);
}

#[test]
fn a_settle_waits_for_everything_to_settle_then_50_ms() {
    use hangul::Key;
    let mut case = Case::new(0, &[ClientKind::Gtk4]);
    case.latin = false;
    case.actions = vec![
        UserAction::Key(Key::Char('r')),
        UserAction::Settle,
        UserAction::Key(Key::Char('k')),
    ];
    // Choice 1 everywhere: the engine takes 2 ms over a key, and the user
    // would come back after 1 ms.
    let mut trace = Vec::new();
    run(&case, Choices::replay(vec![1; 10_000]), &mut trace);
    let user = |text: &str| {
        trace
            .iter()
            .position(|line| line.place == sched::Place::User && line.text == text)
    };
    let settle = user("Settle").expect("the settle");
    let key = user("Key(Char('k'))").expect("the second key");
    let at = trace[settle].at;
    assert!(trace[..settle].iter().all(|line| line.at <= at));
    assert!(trace[..settle]
        .iter()
        .any(|line| line.place == sched::Place::PToC));
    assert_eq!(key, settle + 1, "nothing happens while the user waits");
    assert_eq!(trace[key].at, at + clock::Micros::from_millis(50));
}

#[test]
fn a_report_names_the_seed_the_tier_and_the_field_with_a_trace() {
    let case = Case::new(7, &[ClientKind::Chromium]);
    let mut trace = Vec::new();
    let outcome = run(&case, case.choices(), &mut trace);
    let failure = Failure {
        tier: Tier::Intent,
        check: "text",
        field: Some(1),
        detail: "intended: x\nclient:   y".to_owned(),
    };
    let report = report(&case, &outcome, &[failure], &trace);
    let mut lines = report.lines();
    let heading = lines.next().expect("a heading");
    assert!(heading.starts_with("DST FAILURE seed=7 mode=settled client=chromium"));
    assert_eq!(lines.next(), Some("  check: tier2 text, field Some(1)"));
    assert_eq!(lines.next(), Some("    intended: x"));
    assert!(report.contains(&format!("    {}\n", trace[trace.len() - 1])));
    assert!(report.contains("SOMMELIER_DST_SEED=7 SOMMELIER_DST_CASES=1"));
}

#[test]
fn an_even_seed_is_free_and_an_odd_one_settled() {
    let free = Case::new(10, &ClientKind::ALL);
    let settled = Case::new(11, &ClientKind::ALL);
    assert!(!free.settled && settled.settled);
    assert!(user::settled(&settled.actions));
    assert_eq!(
        Case::new(10, &ClientKind::ALL),
        free,
        "a seed fixes its case"
    );
}
