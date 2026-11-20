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
//!   window INVARIANTS "A press ends the composition on its surface"
//!   says the press fence cannot close);
//! - between the text and the callback: the reset clears the buffer, and
//!   the key goes out handled, with no text;
//! - after the callback: the reset cancels what the key left.
//!
//! IMA resets the engine itself ([`EngineIn::Reset`]) only if it was
//! composing (`ResetContext`), and the engine reaches that reset after
//! every key handed over before it.
//!
//! The engine is the Korean one: it composes with [`Composer`], except that
//! Space ending a composition is part of the commit and handled, as the
//! ChromeOS Korean engine does in the host trace that the proxy's test
//! `held_space_repeats_after_ime_commit_until_physical_release` records,
//! and it handles no release.
//!
//! The user can also ask the engine for more than keys ([`Request`]): to
//! accept its suggestion for the word before the cursor, which it deletes
//! and commits anew, and only while nothing is composed (E10); or to
//! reconvert the text before the cursor, which becomes a region it
//! composes, until the next key commits it as it is. It learns the text
//! from Exo, which hands it the text before the cursor with the request. A Latin layout has no engine
//! ([`Exo::latin`](super::exo::Exo)), and its keys never come here.

use super::clock::Micros;
use super::editor::{reconvertible, word_before};
use super::hangul::{Composer, Key, Output};
use super::sched::Choose;
use std::collections::VecDeque;

/// What Exo hands the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EngineIn {
    /// A key's press or release, with the ID its answer carries.
    Key { key: Key, pressed: bool, id: u64 },
    /// Drop the composition (`Engine::Reset`).
    Reset,
    /// A request, with the ID its answer carries and the text before the
    /// cursor, as Exo predicts it when it hands the request over.
    Request {
        request: Request,
        id: u64,
        before: String,
    },
}

/// What the user asks of the engine besides keys (D6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Request {
    /// Accept the suggestion for the word before the cursor
    /// ([`word_before`]): the word and a `~`.
    AcceptSuggestion,
    /// Reconvert the character before the cursor, or before the region
    /// reconverted so far.
    Reconvert,
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
    /// Delete the text before and after the cursor, which happens only
    /// while nothing is composed (E10). The engine counts UTF-16 code
    /// units, as the IME API does.
    Delete { before: usize, after: usize },
    /// Compose the existing text from before the cursor to after it
    /// (`SetComposingRange`), in UTF-16 code units.
    Region { before: usize, after: usize },
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
    lag: Lag,
    composer: Composer,
    /// The reconverted region, which is the composition instead.
    region: Option<String>,
    /// The region before the last reconversion, which the field keeps if
    /// Exo refuses that reconversion's range.
    unreconverted: Option<String>,
    /// What the engine has not reached yet, in order.
    queue: VecDeque<EngineIn>,
    busy: Option<InService>,
}

impl Engine {
    pub fn new(lag: Lag) -> Self {
        Self {
            lag,
            composer: Composer::default(),
            region: None,
            unreconverted: None,
            queue: VecDeque::new(),
            busy: None,
        }
    }

    /// Exo hands the engine `input` at `now`.
    pub fn send(&mut self, now: Micros, input: EngineIn, choices: &mut impl Choose) {
        self.queue.push_back(input);
        self.serve(now, choices);
    }

    /// The engine's composition, as of the last key it reached; empty if
    /// none.
    pub fn composition(&self) -> String {
        match &self.region {
            Some(region) => region.clone(),
            None => self.composer.preedit(),
        }
    }

    /// The engine learns the text of the region it reconverted, as the
    /// field holds it, from the field's text that follows, once Exo
    /// composed it. Its request measured the region in the text it was
    /// handed, which can lag behind the field's; a region Exo refused
    /// leaves what the engine had ([`Engine::refused_region`]).
    pub fn learn_region(&mut self, text: String) {
        if self.region.is_some() {
            self.region = (!text.is_empty()).then_some(text);
        }
    }

    /// Exo refused the range of the last reconversion, which lay past its
    /// text when the range came, as a report that moved the cursor can
    /// leave it: IMA's `SetComposingRange` returns false before it changes
    /// anything (`input_method_ash.cc`, `text_range.Contains`), so the
    /// field composes what it did, and the engine keeps the region it had.
    pub fn refused_region(&mut self) {
        self.region = self.unreconverted.take();
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
            let (id, (text, handled)) = match self.queue.pop_front() {
                None => return,
                Some(EngineIn::Reset) => {
                    self.composer.reset();
                    self.region = None;
                    continue;
                }
                Some(EngineIn::Key { key, pressed, id }) => (id, self.answer(key, pressed)),
                Some(EngineIn::Request {
                    request,
                    id,
                    before,
                }) => (id, (self.request(request, &before), true)),
            };
            let at = now + self.lag.service(choices);
            self.busy = Some(InService {
                at,
                id,
                text,
                handled,
            });
        }
    }

    /// The answer to a key: a commit if it finished text, a reconverted
    /// region included, then the composition if it differs from the one
    /// shown, which a commit ends; and whether the engine handled the key.
    fn answer(&mut self, key: Key, pressed: bool) -> (Vec<TextAction>, bool) {
        if !pressed {
            return (Vec::new(), false);
        }
        if let Some(region) = self.region.take() {
            let (mut actions, handled) = self.answer(key, pressed);
            actions.insert(0, TextAction::Commit(region));
            return (actions, handled);
        }
        let shown = self.composer.preedit();
        let Output {
            commit,
            preedit,
            consumed,
        } = self.composer.chromeos_key(key);
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

    /// The answer to `request`, for a field with `before` before the cursor:
    /// nothing while it composes jamo, or for a suggestion while it
    /// composes at all.
    fn request(&mut self, request: Request, before: &str) -> Vec<TextAction> {
        if !self.composer.preedit().is_empty() {
            return Vec::new();
        }
        match request {
            Request::AcceptSuggestion if self.region.is_none() => {
                let word = word_before(before);
                if word.is_empty() {
                    return Vec::new();
                }
                vec![
                    TextAction::Delete {
                        before: word.encode_utf16().count(),
                        after: 0,
                    },
                    TextAction::Commit(format!("{word}~")),
                ]
            }
            Request::AcceptSuggestion => Vec::new(),
            Request::Reconvert => {
                let region = self.region.clone().unwrap_or_default();
                let rest = before.strip_suffix(region.as_str());
                let next = rest.and_then(|rest| rest.chars().next_back());
                let Some(c) = next.filter(|&c| reconvertible(c)) else {
                    return Vec::new();
                };
                let region = format!("{c}{region}");
                let before = region.encode_utf16().count();
                self.unreconverted = self.region.replace(region);
                vec![TextAction::Region { before, after: 0 }]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::hangul::keys;

    /// The answers of the engine, with no lag, to `script` (see
    /// [`keys`]): each press's text and whether the engine handled it.
    fn answers(script: &str) -> Vec<(Vec<TextAction>, bool)> {
        let mut engine = Engine::new(Lag::Fast);
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
            answers("rkrk⌫⌫⌫"),
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
            answers("rk  ⏎"),
            [
                (vec![preedit("ㄱ")], true),
                (vec![preedit("가")], true),
                (vec![commit("가 ")], true),
                // With nothing composed, Space and Enter go to the client.
                (vec![], false),
                (vec![], false),
            ]
        );
        assert_eq!(answers("r⏎")[1], (vec![commit("ㄱ")], false));
    }

    /// The engine's answer to `request` for a field with `before` before
    /// the cursor, after it took `script`.
    fn request(script: &str, request: Request, before: &str) -> Vec<TextAction> {
        let mut engine = Engine::new(Lag::Fast);
        let mut inputs: Vec<EngineIn> = (0..)
            .zip(keys(script))
            .map(|(id, key)| press(key, id))
            .collect();
        let id = inputs.len() as u64;
        let before = before.to_owned();
        inputs.push(EngineIn::Request {
            request,
            id,
            before,
        });
        for input in inputs {
            engine.send(Micros(0), input, &mut |_| 0);
        }
        let mut last = Vec::new();
        while engine.ready_at().is_some() {
            match engine.take(&mut |_| 0) {
                EngineOut::Text { id: of, actions } if of == id => last = actions,
                EngineOut::Done { id: of, handled } if of == id => assert!(handled),
                _ => {}
            }
        }
        last
    }

    #[test]
    fn a_suggestion_deletes_the_word_and_commits_it_anew_with_nothing_composed() {
        let accept = Request::AcceptSuggestion;
        let replaced = [
            // In UTF-16 units: two characters.
            TextAction::Delete {
                before: 2,
                after: 0,
            },
            commit("a가~"),
        ];
        assert_eq!(request("", accept, "0 a가"), replaced);
        assert_eq!(request("", accept, "0 "), []);
        assert_eq!(request("rk", accept, "0 a가"), [], "composing");
    }

    #[test]
    fn a_reconversion_grows_a_region_that_the_next_key_commits() {
        let mut engine = Engine::new(Lag::Fast);
        let mut answers = Vec::new();
        let inputs = [
            (Request::Reconvert, "0a가"),
            // The text before the cursor holds the region, which grows over
            // one more character, but not over a marker.
            (Request::Reconvert, "0a가"),
            (Request::Reconvert, "0a가"),
            (Request::AcceptSuggestion, "0a가"),
        ];
        for (id, (request, before)) in (0..).zip(inputs) {
            let before = before.to_owned();
            engine.send(
                Micros(0),
                EngineIn::Request {
                    request,
                    id,
                    before,
                },
                &mut |_| 0,
            );
        }
        engine.send(Micros(0), press(Key::Char('r'), 4), &mut |_| 0);
        while engine.ready_at().is_some() {
            if let EngineOut::Text { actions, .. } = engine.take(&mut |_| 0) {
                answers.push(actions);
            }
        }
        let region = |before| TextAction::Region { before, after: 0 };
        let expected = [
            vec![region(1)],
            vec![region(2)],
            vec![commit("a가"), preedit("ㄱ")],
        ];
        assert_eq!(answers, expected);
        assert_eq!(engine.composition(), "ㄱ");
        // A composition of jamo is not reconverted.
        assert_eq!(request("r", Request::Reconvert, "0aㄱ"), []);
    }

    #[test]
    fn the_engine_learns_the_text_of_its_region_from_the_field() {
        let mut engine = Engine::new(Lag::Fast);
        engine.learn_region("가".to_owned());
        assert_eq!(engine.composition(), "", "it has no region");
        let before = "0a".to_owned();
        let request = Request::Reconvert;
        engine.send(
            Micros(0),
            EngineIn::Request {
                request,
                id: 0,
                before,
            },
            &mut |_| 0,
        );
        while engine.ready_at().is_some() {
            engine.take(&mut |_| 0);
        }
        assert_eq!(engine.composition(), "a");
        engine.learn_region("가".to_owned());
        assert_eq!(engine.composition(), "가");
        engine.learn_region(String::new());
        assert_eq!(engine.composition(), "");
    }

    #[test]
    fn a_reconversion_exo_refused_leaves_the_region_the_engine_had() {
        let mut engine = Engine::new(Lag::Fast);
        let reconvert = |engine: &mut Engine, id| {
            let before = "0a가".to_owned();
            let request = Request::Reconvert;
            let input = EngineIn::Request {
                request,
                id,
                before,
            };
            engine.send(Micros(0), input, &mut |_| 0);
            while engine.ready_at().is_some() {
                engine.take(&mut |_| 0);
            }
        };
        reconvert(&mut engine, 0);
        engine.refused_region();
        assert_eq!(engine.composition(), "", "no region before the first");
        reconvert(&mut engine, 1);
        assert_eq!(engine.composition(), "가");
        reconvert(&mut engine, 2);
        assert_eq!(engine.composition(), "a가");
        engine.refused_region();
        assert_eq!(
            engine.composition(),
            "가",
            "the region before the refused one"
        );
    }

    #[test]
    fn every_release_is_unhandled() {
        let mut engine = Engine::new(Lag::Fast);
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
        let mut engine = Engine::new(Lag::Normal);
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
    fn a_reset_waits_for_the_keys_before_it() {
        let mut engine = Engine::new(Lag::Fast);
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
