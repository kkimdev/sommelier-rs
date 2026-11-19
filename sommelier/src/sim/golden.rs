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

//! Golden traces for the staged switch of the IME core
//! (`docs/IME_REDESIGN.md`, section 4.9).
//!
//! `golden.txt` has a line for each of the first [`SEEDS`] DST cases: the
//! seed, then the count and hash of the choices the case made, and of the
//! messages the proxy sent the guest (`pc`, P→C) and the host (`pe`,
//! P→E). A stream's hash covers each message's bytes and descriptor count,
//! message by message, and its field ends with a checkpoint every
//! [`EVERY`] messages, which locates the first message that differs. A
//! keymap's size depends on the machine's XKB data, so it is not hashed.
//! [`golden_traces_match`] replays each case and compares; with
//! `SOMMELIER_DST_BLESS=1` it writes the file instead.

use super::sched::{Line, Place};
use super::{pinned, run, Case, ClientKind};

const SEEDS: u64 = 2_000;
const EVERY: usize = 16;
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/sim/golden.txt");

/// FNV-1a, 64 bits: unlike `DefaultHasher`, the same in every Rust release.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
}

/// A stream's messages: each as the trace shows it, its bytes and its
/// descriptor count.
type Messages<'a> = Vec<(&'a Line, &'a [u8], usize)>;

/// The messages `wire` of the stream at `place`, each with its line in
/// `trace`.
fn stream<'a>(trace: &'a [Line], place: Place, wire: &'a [(Vec<u8>, usize)]) -> Messages<'a> {
    let lines = trace.iter().filter(|line| line.place == place);
    assert_eq!(
        lines.clone().count(),
        wire.len(),
        "a trace line per message"
    );
    let messages = lines.zip(wire);
    messages
        .map(|(line, (bytes, fds))| (line, &bytes[..], *fds))
        .collect()
}

/// The golden field of `messages`: `count:hash:checkpoints`.
fn field(name: &str, messages: &Messages) -> String {
    let mut hash = Fnv::new();
    let mut checkpoints = String::new();
    for (i, &(line, bytes, fds)) in messages.iter().enumerate() {
        // The length and descriptor count first, so boundaries count.
        hash.write(&[bytes.len().to_le_bytes(), fds.to_le_bytes()].concat());
        let keymap = line.text.contains(" WlKeyboard(Keymap ");
        hash.write(&bytes[..bytes.len() - if keymap { 4 } else { 0 }]);
        if (i + 1) % EVERY == 0 {
            checkpoints += &format!("{:02x}", hash.0 >> 56);
        }
    }
    format!("{name}:{}:{:016x}:{checkpoints}", messages.len(), hash.0)
}

/// Where stream `now` first leaves `golden`, and its messages from there:
/// the checkpoints place the first difference in a run of [`EVERY`]
/// messages, or after the last checkpoint the two share.
fn difference(golden: &str, now: &str, messages: &Messages) -> String {
    let checkpoints = |field: &str| field.rsplit(':').next().unwrap_or("").as_bytes().to_vec();
    let (golden_checks, now_checks) = (checkpoints(golden), checkpoints(now));
    let pairs = golden_checks.chunks(2).zip(now_checks.chunks(2));
    let from = pairs.clone().take_while(|(a, b)| a == b).count() * EVERY;
    let place = match from / EVERY < pairs.count() {
        true => format!("in messages {from} to {}", from + EVERY - 1),
        false => format!("at message {from} or later"),
    };
    let mut text = format!("  golden {golden}\n  now    {now}\n");
    text += &format!("  first difference {place}; the proxy now sends:\n");
    for (i, (line, bytes, fds)) in messages.iter().enumerate().skip(from).take(EVERY) {
        text += &format!("    #{i} {line}\n          {fds} fds, bytes {bytes:02x?}\n");
    }
    text
}

#[test]
fn golden_traces_match() {
    pinned("sim::golden::golden_traces_match", || {
        let bless = std::env::var_os("SOMMELIER_DST_BLESS").is_some();
        let golden = match bless {
            true => String::new(),
            false => std::fs::read_to_string(GOLDEN).expect("read sim/golden.txt"),
        };
        let mut golden = golden.lines();
        let (mut blessed, mut failed, mut first) = (String::new(), Vec::new(), String::new());
        for seed in 0..SEEDS {
            let case = Case::new(seed, &ClientKind::ALL);
            let mut trace = Vec::new();
            let outcome = run(&case, case.choices(), &mut trace);
            let mut choices = Fnv::new();
            for choice in &outcome.choices {
                choices.write(&choice.to_le_bytes());
            }
            let n = outcome.choices.len();
            let [pc, pe] = &outcome.wire;
            let streams = [
                stream(&trace, Place::PToC, pc),
                stream(&trace, Place::PToE, pe),
            ];
            let fields = [field("pc", &streams[0]), field("pe", &streams[1])];
            let line = format!(
                "{seed} choices:{n}:{:016x} {} {}",
                choices.0, fields[0], fields[1]
            );
            blessed += &format!("{line}\n");
            let want = golden.next().unwrap_or_default();
            if want == line {
                continue;
            }
            failed.push(seed);
            if failed.len() > 1 {
                continue;
            }
            first = format!("seed {seed}:\n");
            for (i, (want, now)) in want.split(' ').zip(line.split(' ')).enumerate() {
                first += &match (i, want == now) {
                    (_, true) => String::new(),
                    (2 | 3, false) => difference(want, now, &streams[i - 2]),
                    _ => format!("  golden {want}\n  now    {now}\n"),
                };
            }
        }
        if bless {
            std::fs::write(GOLDEN, blessed).expect("write sim/golden.txt");
            return;
        }
        assert_eq!(golden.count(), 0, "golden.txt has more than {SEEDS} lines");
        assert!(
            failed.is_empty(),
            "{} of {SEEDS} seeds left their golden trace, seeds {:?}; the first:\n{first}\
             If the change is meant (never from C3-pre to C3c), bless it: \
             SOMMELIER_DST_BLESS=1 cargo test -p sommelier --bins sim::golden",
            failed.len(),
            &failed[..failed.len().min(20)]
        );
    });
}
