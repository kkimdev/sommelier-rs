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

//! The choices that fix a case.
//!
//! Every model makes each choice it leaves open, such as a latency or a
//! host behaviour no source settles, through [`Choose`], so that a recorded
//! stream of choices replays a case exactly. Choice 0 is always the
//! shortest latency or the first variant, so an all-zero stream is the
//! most sequential schedule.
//!
//! A case's messages travel FIFO [`Channel`]s, each delivered when its
//! drawn latency has passed, and [`earliest`] picks what happens next, so
//! the only reorderings are across channels, never within one.

use std::collections::VecDeque;

use super::clock::Micros;
use crate::test_support::Rng;

/// The source of a case's choices.
pub(super) trait Choose {
    /// A choice below `n`.
    fn pick(&mut self, n: u32) -> u32;

    /// A value from `low` to `high`, both included; choice 0 is `low`.
    fn between(&mut self, low: u64, high: u64) -> u64 {
        let span = u32::try_from(high - low + 1).expect("a span that fits a choice");
        low + u64::from(self.pick(span))
    }
}

/// A function of `n` is a source too, for tests that script the choices.
impl<F: FnMut(u32) -> u32> Choose for F {
    fn pick(&mut self, n: u32) -> u32 {
        self(n)
    }
}

/// A case's choices, drawn from a seed and recorded, or replayed from a
/// recording, so that a shrinker can edit and replay them.
pub(super) struct Choices {
    recorded: Vec<u32>,
    /// The choices to replay; draws past their end are 0.
    replay: Option<Vec<u32>>,
    rng: Rng,
}

impl Choices {
    /// Choices drawn from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            recorded: Vec::new(),
            replay: None,
            rng: Rng::new(seed),
        }
    }

    /// Choices that replay `choices`, each taken modulo the number of
    /// options of its draw.
    pub fn replay(choices: Vec<u32>) -> Self {
        Self {
            replay: Some(choices),
            ..Self::new(0)
        }
    }

    /// The choices made so far, in order.
    pub fn recorded(&self) -> &[u32] {
        &self.recorded
    }
}

impl Choose for Choices {
    fn pick(&mut self, n: u32) -> u32 {
        let choice = match &self.replay {
            Some(replay) => replay.get(self.recorded.len()).map_or(0, |c| c % n),
            None => (self.rng.next_u64() % u64::from(n)) as u32,
        };
        self.recorded.push(choice);
        choice
    }
}

/// A FIFO channel whose messages each arrive after a drawn latency, but
/// never before the message ahead of them.
pub(super) struct Channel<T> {
    /// The longest latency a message draws.
    latency: Micros,
    queue: VecDeque<(Micros, T)>,
}

impl<T> Channel<T> {
    /// A channel whose latencies are drawn from 0 to `latency`.
    pub fn new(latency: Micros) -> Self {
        Self {
            latency,
            queue: VecDeque::new(),
        }
    }

    /// Sends `message` at `now`.
    pub fn send(&mut self, now: Micros, message: T, choices: &mut impl Choose) {
        let drawn = now + Micros(choices.between(0, self.latency.0));
        let ahead = self.queue.back().map_or(drawn, |&(ready, _)| ready);
        self.queue.push_back((drawn.max(ahead), message));
    }

    /// When the message at the head arrives, if there is one.
    pub fn ready_at(&self) -> Option<Micros> {
        self.queue.front().map(|&(ready, _)| ready)
    }

    /// The message at the head.
    ///
    /// # Panics
    ///
    /// If the channel is empty.
    pub fn take(&mut self) -> T {
        self.queue.pop_front().expect("a message in the channel").1
    }
}

/// Which of the sources, each ready at its time or idle (`None`), goes
/// next: the earliest; of several ready at once, a choice among them in
/// the sources' order, so choice 0 is the first.
pub(super) fn earliest(sources: &[Option<Micros>], choices: &mut impl Choose) -> Option<usize> {
    let first = sources.iter().flatten().min()?;
    let tied: Vec<usize> = (0..sources.len())
        .filter(|&i| sources[i] == Some(*first))
        .collect();
    let pick = match tied.len() {
        1 => 0,
        n => choices.pick(u32::try_from(n).expect("a few sources")) as usize,
    };
    Some(tied[pick])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_replay_what_they_recorded_and_zero_past_the_end() {
        let mut drawn = Choices::new(7);
        let first: Vec<u32> = (0..20).map(|_| drawn.pick(5)).collect();
        assert!(first.iter().any(|&c| c != 0) && first.iter().all(|&c| c < 5));
        assert_eq!(drawn.recorded(), first);
        let mut replayed = Choices::replay(first.clone());
        let again: Vec<u32> = (0..20).map(|_| replayed.pick(5)).collect();
        assert_eq!(again, first);
        // A replayed choice is taken modulo the draw's options, and a draw
        // past the recording is 0, the most sequential.
        let mut edited = Choices::replay(vec![7]);
        assert_eq!((edited.pick(3), edited.pick(3)), (1, 0));
    }

    #[test]
    fn a_channel_keeps_its_order_whatever_the_latencies() {
        let mut channel = Channel::new(Micros(100));
        // The first draws the longest latency and the second none, which
        // would overtake it on a channel that is not FIFO.
        let mut latencies = [100, 0].into_iter();
        let mut choices = |_| latencies.next().unwrap();
        channel.send(Micros(0), 'a', &mut choices);
        channel.send(Micros(10), 'b', &mut choices);
        assert_eq!(channel.ready_at(), Some(Micros(100)));
        assert_eq!(channel.take(), 'a');
        assert_eq!(channel.ready_at(), Some(Micros(100)));
        assert_eq!(channel.take(), 'b');
        assert_eq!(channel.ready_at(), None);
    }

    #[test]
    fn the_earliest_source_goes_and_a_choice_breaks_a_tie() {
        let sources = [None, Some(Micros(5)), Some(Micros(3)), Some(Micros(3))];
        let mut never = |_| panic!("no tie to break");
        assert_eq!(earliest(&sources[..2], &mut never), Some(1));
        assert_eq!(earliest(&sources, &mut |n| n - 1), Some(3));
        assert_eq!(earliest(&sources, &mut |_| 0), Some(2));
        assert_eq!(earliest(&[None, None], &mut never), None);
    }
}
