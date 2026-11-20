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

//! The simulation's virtual clock.
//!
//! Everything in a case happens at a time on one clock that only the
//! scheduler advances, so a time-based rule, such as Chromium ignoring
//! the input method for 100 ms after a reset, behaves the same on every
//! run of a seed.

use std::ops::Add;

/// A time on the virtual clock, counted from the start of the case, or a
/// span of it; in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Micros(pub u64);

impl Micros {
    /// A span of `ms` milliseconds.
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms * 1_000)
    }
}

impl Add for Micros {
    type Output = Self;

    fn add(self, span: Self) -> Self {
        Self(self.0 + span.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_plus_a_span_is_a_later_time() {
        let at = Micros(250) + Micros::from_millis(100);
        assert_eq!(at, Micros(100_250));
        assert!(Micros(250) < at);
    }
}
