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
//! `InputMethodAsh` (`ui/base/ime/ash/input_method_ash.cc`, IMA below)
//! hands each key event, press or release, to the engine and gets its
//! answer in two parts: first the text the key made (`CommitText`,
//! `UpdateCompositionText`), which IMA buffers while the key is in flight,
//! then the callback (`ProcessKeyEventDone`), which says whether the engine
//! handled the key and sends the buffered text on with it. The engine works
//! through what it is handed in order, one key at a time: it answers a key
//! from its composition as it stands when it reaches the key, sends the
//! text after a service time drawn from the case's [`Lag`], and the
//! callback a drawn gap later.
//!
//! IMA's reset (`ResetContext`) can fall anywhere in that, with one of
//! three outcomes for a key in flight:
//! - before the key's text: the text arrives after the reset, made from the
//!   composition before it, and goes out with the callback, late (the
//!   window INVARIANTS "A pointer press ends the composition on its
//!   surface" says the press fence cannot close);
//! - between the text and the callback: the reset clears the buffer, and
//!   the key goes out handled, with no text;
//! - after the callback: the reset cancels what the key left.
//!
//! IMA resets the engine itself ([`EngineIn::Reset`]) only if it was
//! composing (`ResetContext`), and the engine reaches that reset after
//! every key handed over before it.
//!
//! In Korean mode the engine composes with [`Composer`], except that Space
//! ending a composition is part of the commit and handled, as the ChromeOS
//! Korean engine does in the host trace that the proxy's test
//! `held_space_repeats_after_ime_commit_until_physical_release` records. In
//! Latin mode it handles no key, and it handles no release in either.

use super::clock::Micros;
use super::hangul::{Composer, Key, Output};
use super::sched::Choose;
use std::collections::VecDeque;

/// What Exo hands the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EngineIn {
    /// A key's press or release, with the ID its answer carries.
    Key { key: Key, pressed: bool, id: u64 },
    /// Drop the composition (`Engine::Reset`).
    Reset,
}

/// A part of the engine's answer to a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EngineOut {
    /// The text key `id` made, in order; never empty.
    Text { id: u64, actions: Vec<TextAction> },
    /// The callback: whether the engine handled key `id`.
    Done { id: u64, handled: bool },
}

/// An edit of the engine's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TextAction {
    /// Commit this text, which ends the composition.
    Commit(String),
    /// Show this composition; an empty one ends it.
    Preedit(String),
}

/// What the engine does with keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// The Korean two-set input method.
    Korean,
    /// An input method that handles no key, as for English.
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

/// The gap between a key's text and its callback, which travel the same
/// mojo pipe: up to 1 ms.
fn gap(choices: &mut impl Choose) -> Micros {
    Micros(choices.between(0, 1_000))
}

/// The key in service.
struct InService {
    /// When its next part is ready.
    at: Micros,
    id: u64,
    /// Its text, until sent.
    text: Vec<TextAction>,
    handled: bool,
}

/// The engine, with what Exo handed it that it has not answered yet.
pub(super) struct Engine {
    mode: Mode,
    lag: Lag,
    composer: Composer,
    /// What the engine has not reached yet, in order.
    queue: VecDeque<EngineIn>,
    busy: Option<InService>,
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

    /// When the next part of the answer to the key in service is ready;
    /// `None` while the engine has nothing to do.
    pub fn ready_at(&self) -> Option<Micros> {
        self.busy.as_ref().map(|key| key.at)
    }

    /// The next part of the answer to the key in service, at
    /// [`Engine::ready_at`]: its text if it made any, then its callback,
    /// after which the engine starts on what follows.
    pub fn take(&mut self, choices: &mut impl Choose) -> EngineOut {
        let mut key = self.busy.take().expect("a key in service");
        if !key.text.is_empty() {
            let (id, actions) = (key.id, std::mem::take(&mut key.text));
            key.at = key.at + gap(choices);
            self.busy = Some(key);
            return EngineOut::Text { id, actions };
        }
        self.serve(key.at, choices);
        EngineOut::Done {
            id: key.id,
            handled: key.handled,
        }
    }

    /// Works through the queue from `now` until a key is in service.
    fn serve(&mut self, now: Micros, choices: &mut impl Choose) {
        while self.busy.is_none() {
            match self.queue.pop_front() {
                None => return,
                Some(EngineIn::Reset) => self.composer.reset(),
                Some(EngineIn::Key { key, pressed, id }) => {
                    let (text, handled) = self.answer(key, pressed);
                    let at = now + self.lag.service(choices);
                    self.busy = Some(InService {
                        at,
                        id,
                        text,
                        handled,
                    });
                }
            }
        }
    }

    /// The answer to a key: a commit if it finished text, then the
    /// composition if it differs from the one shown, which a commit ends;
    /// and whether the engine handled the key.
    fn answer(&mut self, key: Key, pressed: bool) -> (Vec<TextAction>, bool) {
        if !pressed || self.mode == Mode::Latin {
            return (Vec::new(), false);
        }
        let shown = self.composer.preedit();
        let Output {
            mut commit,
            preedit,
            mut consumed,
        } = self.composer.key(key);
        if key == Key::Char(' ') && !commit.is_empty() {
            commit.push(' ');
            consumed = true;
        }
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
        (actions, consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::hangul::keys;

    /// The answers of an engine in `mode`, with no lag, to `script` (see
    /// [`keys`]): each press's text and whether the engine handled it.
    fn answers(mode: Mode, script: &str) -> Vec<(Vec<TextAction>, bool)> {
        let mut engine = Engine::new(mode, Lag::Fast);
        let mut answers = Vec::new();
        for (id, key) in (0..).zip(keys(script)) {
            engine.send(Micros(0), press(key, id), &mut |_| 0);
            let mut text = Vec::new();
            loop {
                match engine.take(&mut |_| 0) {
                    EngineOut::Text { id: of, actions } => {
                        assert_eq!(of, id);
                        text = actions;
                    }
                    EngineOut::Done { id: of, handled } => {
                        assert_eq!(of, id);
                        answers.push((text, handled));
                        break;
                    }
                }
            }
        }
        answers
    }

    fn press(key: Key, id: u64) -> EngineIn {
        EngineIn::Key {
            key,
            pressed: true,
            id,
        }
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
            answers(Mode::Korean, "rkrk⌫⌫⌫"),
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
    }

    #[test]
    fn space_ending_a_composition_is_committed_with_it() {
        assert_eq!(
            answers(Mode::Korean, "rk  ⏎"),
            [
                (vec![preedit("ㄱ")], true),
                (vec![preedit("가")], true),
                (vec![commit("가 ")], true),
                // With nothing composed, Space and Enter go to the client.
                (vec![], false),
                (vec![], false),
            ]
        );
        assert_eq!(answers(Mode::Korean, "r⏎")[1], (vec![commit("ㄱ")], false));
    }

    #[test]
    fn latin_mode_and_every_release_are_unhandled() {
        assert_eq!(
            answers(Mode::Latin, "rk"),
            [(vec![], false), (vec![], false)]
        );
        let mut engine = Engine::new(Mode::Korean, Lag::Fast);
        let release = EngineIn::Key {
            key: Key::Char('r'),
            pressed: false,
            id: 0,
        };
        engine.send(Micros(0), release, &mut |_| 0);
        let done = EngineOut::Done {
            id: 0,
            handled: false,
        };
        assert_eq!(engine.take(&mut |_| 0), done);
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
    fn the_text_comes_after_the_service_time_and_the_callback_a_gap_later() {
        let mut engine = Engine::new(Mode::Korean, Lag::Normal);
        // Every service time and gap at its largest.
        let mut choices = |n: u32| n - 1;
        for (id, key) in (0..).zip(keys("rk")) {
            engine.send(Micros(0), press(key, id), &mut choices);
        }
        let mut parts = Vec::new();
        while let Some(at) = engine.ready_at() {
            parts.push((at, engine.take(&mut choices)));
        }
        let text = |id, text: &str| EngineOut::Text {
            id,
            actions: vec![preedit(text)],
        };
        let done = |id| EngineOut::Done { id, handled: true };
        // The second key waits for the first's callback.
        let expected = [
            (Micros(15_000), text(0, "ㄱ")),
            (Micros(16_000), done(0)),
            (Micros(31_000), text(1, "가")),
            (Micros(32_000), done(1)),
        ];
        assert_eq!(parts, expected);
    }

    #[test]
    fn a_reset_cancels_no_key_handed_over_before_it() {
        let mut engine = Engine::new(Mode::Korean, Lag::Fast);
        let mut choices = |_| 0;
        for (id, key) in (0..).zip(keys("rk")) {
            engine.send(Micros(0), press(key, id), &mut choices);
        }
        engine.send(Micros(1), EngineIn::Reset, &mut choices);
        engine.send(Micros(2), press(Key::Char('k'), 2), &mut choices);
        let mut texts = Vec::new();
        while engine.ready_at().is_some() {
            if let EngineOut::Text { actions, .. } = engine.take(&mut choices) {
                texts.push(actions);
            }
        }
        // The second is made from the composition before the reset.
        let expected = [[preedit("ㄱ")], [preedit("가")], [preedit("ㅏ")]];
        assert_eq!(texts, expected);
    }
}
