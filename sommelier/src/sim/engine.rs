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

//! The input method engine behind Exo, which answers each key after a
//! while.
//!
//! Chrome hands each key to the engine and acts on the answer when it
//! arrives (`ProcessKeyEventDone`). The engine works through what Exo
//! hands it in order, one key at a time: it answers a key from its
//! composition as it stands when it reaches the key, and the answer
//! arrives after a service time drawn from the case's [`Lag`]. A reset
//! takes effect when the engine reaches it, so it cancels no key handed
//! over before it: that key's answer, made from the old composition,
//! arrives after Exo has handled the reset. Chrome's `CancelComposition`
//! behaves the same (INVARIANTS, "A pointer press ends the composition on
//! its surface"), and the simulation must show the proxy coping with it.
//!
//! In Korean mode the engine composes with [`Composer`]; in Latin mode it
//! passes every key to the application.

use super::clock::Micros;
use super::hangul::{Composer, Key, Output};
use super::sched::Choose;
use std::collections::VecDeque;

/// What Exo hands the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EngineIn {
    /// A key press, with the ID its answer carries.
    Key { key: Key, id: u64 },
    /// Drop the composition. Exo sends it for a `reset` and for a detach,
    /// whose confirmation Exo makes itself from the composition it tracks.
    Reset,
}

/// The engine's answer to a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EngineOut {
    pub id: u64,
    /// The edits the key made, in order.
    pub actions: Vec<TextAction>,
    /// Whether the engine used the key; a key it did not use reaches the
    /// application after the edits.
    pub consumed: bool,
}

/// An edit of the engine's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TextAction {
    /// Commit this text, which ends the composition.
    Commit(String),
    /// Show this composition; an empty one ends it.
    Preedit(String),
    /// Delete bytes before and after the cursor, which the engine does only
    /// while it composes nothing. No key makes one yet.
    Delete { before: usize, after: usize },
}

/// What the engine does with keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// The Korean two-set input method.
    Korean,
    /// An input method that passes every key, as for English.
    Latin,
}

/// How long the engine takes over a key, drawn once per case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lag {
    /// 0.2 to 2 ms.
    Fast,
    /// 2 to 15 ms.
    Normal,
    /// 15 to 60 ms, with a spike to 150 ms one key in 20.
    Slow,
}

impl Lag {
    pub fn draw(choices: &mut impl Choose) -> Self {
        [Self::Fast, Self::Normal, Self::Slow][choices.pick(3) as usize]
    }

    /// A key's service time.
    fn service(self, choices: &mut impl Choose) -> Micros {
        Micros(match self {
            Self::Fast => choices.between(200, 2_000),
            Self::Normal => choices.between(2_000, 15_000),
            // Choice 0 is no spike.
            Self::Slow if choices.pick(20) == 19 => 150_000,
            Self::Slow => choices.between(15_000, 60_000),
        })
    }
}

/// The engine, with what Exo handed it that it has not answered yet.
pub(super) struct Engine {
    mode: Mode,
    lag: Lag,
    composer: Composer,
    /// What the engine has not reached yet, in order.
    queue: VecDeque<EngineIn>,
    /// The key in service: when its answer is ready, and the answer.
    busy: Option<(Micros, EngineOut)>,
}

impl Engine {
    pub fn new(mode: Mode, lag: Lag) -> Self {
        Self {
            mode,
            lag,
            composer: Composer::default(),
            queue: VecDeque::new(),
            busy: None,
        }
    }

    /// Exo hands the engine `input` at `now`.
    pub fn send(&mut self, now: Micros, input: EngineIn, choices: &mut impl Choose) {
        self.queue.push_back(input);
        self.serve(now, choices);
    }

    /// When the answer to the key in service is ready; `None` while the
    /// engine has nothing to do.
    pub fn ready_at(&self) -> Option<Micros> {
        self.busy.as_ref().map(|&(at, _)| at)
    }

    /// The answer to the key in service, at [`Engine::ready_at`]. The
    /// engine then starts on what follows.
    pub fn take(&mut self, choices: &mut impl Choose) -> EngineOut {
        let (at, out) = self.busy.take().expect("a key in service");
        self.serve(at, choices);
        out
    }

    /// Works through the queue from `now` until a key is in service.
    fn serve(&mut self, now: Micros, choices: &mut impl Choose) {
        while self.busy.is_none() {
            match self.queue.pop_front() {
                None => return,
                Some(EngineIn::Reset) => self.composer.reset(),
                Some(EngineIn::Key { key, id }) => {
                    let out = self.answer(key, id);
                    self.busy = Some((now + self.lag.service(choices), out));
                }
            }
        }
    }

    /// The answer to `key`: a commit if it finished text, then the
    /// composition if it differs from the one shown, which a commit ends.
    fn answer(&mut self, key: Key, id: u64) -> EngineOut {
        if self.mode == Mode::Latin {
            return EngineOut {
                id,
                actions: Vec::new(),
                consumed: false,
            };
        }
        let shown = self.composer.preedit();
        let Output {
            commit,
            preedit,
            consumed,
        } = self.composer.key(key);
        let mut actions = Vec::new();
        let shown = if commit.is_empty() {
            shown
        } else {
            actions.push(TextAction::Commit(commit));
            String::new()
        };
        if preedit != shown {
            actions.push(TextAction::Preedit(preedit));
        }
        EngineOut {
            id,
            actions,
            consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::hangul::keys;

    /// The answers of an engine in `mode`, with no lag, to `script` (see
    /// [`keys`]), each with its key's position as its ID.
    fn answers(mode: Mode, script: &str) -> Vec<(Vec<TextAction>, bool)> {
        let mut engine = Engine::new(mode, Lag::Fast);
        let mut answers = Vec::new();
        for (id, key) in (0..).zip(keys(script)) {
            engine.send(Micros(0), EngineIn::Key { key, id }, &mut |_| 0);
            let out = engine.take(&mut |_| 0);
            assert_eq!(out.id, id);
            answers.push((out.actions, out.consumed));
        }
        answers
    }

    fn preedit(text: &str) -> TextAction {
        TextAction::Preedit(text.to_owned())
    }

    fn commit(text: &str) -> TextAction {
        TextAction::Commit(text.to_owned())
    }

    #[test]
    fn korean_mode_composes_and_sends_only_what_changed() {
        assert_eq!(
            answers(Mode::Korean, "rkrk⌫⌫ "),
            [
                (vec![preedit("ㄱ")], true),
                (vec![preedit("가")], true),
                (vec![preedit("각")], true),
                // A commit ends the composition, so the syllable that
                // follows is shown again.
                (vec![commit("가"), preedit("가")], true),
                (vec![preedit("ㄱ")], true),
                (vec![preedit("")], true),
                (vec![], false),
            ]
        );
        assert_eq!(
            answers(Mode::Korean, "rk "),
            [
                (vec![preedit("ㄱ")], true),
                (vec![preedit("가")], true),
                (vec![commit("가")], false),
            ]
        );
    }

    #[test]
    fn latin_mode_passes_every_key() {
        assert_eq!(
            answers(Mode::Latin, "rk"),
            [(vec![], false), (vec![], false)]
        );
    }

    #[test]
    fn each_lag_bounds_a_keys_service_time() {
        let cases = [
            (Lag::Fast, [200, 2_000]),
            (Lag::Normal, [2_000, 15_000]),
            (Lag::Slow, [15_000, 60_000]),
        ];
        for (lag, bounds) in cases {
            for (choice, bound) in [0, u32::MAX].into_iter().zip(bounds) {
                // The largest choice below n, except for the spike's.
                let mut choices = |n| if n == 20 { 0 } else { choice.min(n - 1) };
                assert_eq!(lag.service(&mut choices), Micros(bound), "{lag:?}");
            }
        }
        assert_eq!(Lag::Slow.service(&mut |n: u32| n - 1), Micros(150_000));
        let lags = [0, 1, 2].map(|choice| Lag::draw(&mut |_| choice));
        assert_eq!(lags, [Lag::Fast, Lag::Normal, Lag::Slow]);
    }

    #[test]
    fn the_engine_answers_one_key_at_a_time_in_order() {
        let mut engine = Engine::new(Mode::Korean, Lag::Normal);
        let mut choices = |_| 0;
        for (id, key) in (0..).zip(keys("rk")) {
            engine.send(Micros(0), EngineIn::Key { key, id }, &mut choices);
        }
        assert_eq!(engine.ready_at(), Some(Micros(2_000)));
        assert_eq!(engine.take(&mut choices).id, 0);
        // The second key waited for the first.
        assert_eq!(engine.ready_at(), Some(Micros(4_000)));
        assert_eq!(engine.take(&mut choices).id, 1);
        assert_eq!(engine.ready_at(), None);
    }

    #[test]
    fn a_reset_cancels_no_key_handed_over_before_it() {
        let mut engine = Engine::new(Mode::Korean, Lag::Fast);
        let mut choices = |_| 0;
        for (id, key) in (0..).zip(keys("rk")) {
            engine.send(Micros(0), EngineIn::Key { key, id }, &mut choices);
        }
        engine.send(Micros(1), EngineIn::Reset, &mut choices);
        engine.send(
            Micros(2),
            EngineIn::Key {
                key: Key::Char('k'),
                id: 2,
            },
            &mut choices,
        );
        let mut take = || engine.take(&mut choices).actions;
        assert_eq!(take(), [preedit("ㄱ")]);
        // Made from the composition before the reset, and late.
        assert_eq!(take(), [preedit("가")]);
        assert_eq!(take(), [preedit("ㅏ")]);
    }
}
