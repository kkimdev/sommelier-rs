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

//! Golden traces for the staged switch to the IME core (the design's
//! section 4.9).
//!
//! A golden is what the proxy sent in a case, P→C and P→E in order, or a
//! hash of it. Until the switch, the DST records them; C3-pre commits
//! them, blessed on the pre-switch code, and each stage of the switch must
//! reproduce them exactly.
//!
//! `SOMMELIER_DST_GOLDEN=<n>` makes [`golden_traces_are_recorded`] print
//! the golden line of seeds 0 to n - 1 (4 by default), and
//! `SOMMELIER_DST_GOLDEN_TRACE=1` the whole traces too.

use super::sched::{Line, Place};
use super::{env_u64, pinned, run, Case, ClientKind};

/// What the proxy sent in `trace`, in order, without times.
pub(super) fn sent(trace: &[Line]) -> Vec<String> {
    trace
        .iter()
        .filter(|line| matches!(line.place, Place::PToC | Place::PToE))
        .map(|line| format!("{} {}", line.place, line.text))
        .collect()
}

/// The 64-bit FNV-1a hash of `lines`, each ended by a newline: stable
/// across Rust releases, unlike the standard library's hasher.
pub(super) fn hash(lines: &[String]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in lines.iter().flat_map(|line| line.bytes().chain(*b"\n")) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The golden of `seed`: what the proxy sent in its case.
pub(super) fn record(seed: u64) -> Vec<String> {
    let case = Case::new(seed, &ClientKind::ALL);
    let mut trace = Vec::new();
    run(&case, case.choices(), &mut trace);
    sent(&trace)
}

#[test]
fn golden_traces_are_recorded() {
    pinned("sim::golden::golden_traces_are_recorded", || {
        let full = std::env::var_os("SOMMELIER_DST_GOLDEN_TRACE").is_some();
        for seed in 0..env_u64("SOMMELIER_DST_GOLDEN", 4) {
            let golden = record(seed);
            assert_eq!(record(seed), golden, "seed {seed} records the same twice");
            assert!(!golden.is_empty());
            println!("{seed} {:016x} {}", hash(&golden), golden.len());
            if full {
                golden.iter().for_each(|line| println!("  {line}"));
            }
        }
    });
}

#[test]
fn the_hash_is_fnv_1a_of_the_lines() {
    assert_eq!(hash(&[]), 0xcbf2_9ce4_8422_2325);
    // FNV-1a of "a\n".
    assert_eq!(hash(&["a".to_owned()]), 0x089b_dc07_b544_e7b2);
    assert_ne!(
        hash(&["ab".to_owned()]),
        hash(&["a".to_owned(), "b".to_owned()])
    );
}
