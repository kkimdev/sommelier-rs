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
