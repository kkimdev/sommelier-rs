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

//! Messages logged once per process.
//!
//! A condition every connection meets alike, such as a host that lacks a
//! protocol, is logged once per process, not once per connection or per
//! event. [`WarnOnce`] is the one flag for that: a `static` next to the
//! code that logs, and a value of a test's own in a test, through a
//! function that takes the flag, so that no test shares or resets the
//! process's.

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether a once-per-process message has been logged.
#[derive(Debug, Default)]
pub(crate) struct WarnOnce(AtomicBool);

impl WarnOnce {
    pub(crate) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Return `true` the first time it is asked, and `false` after: whether
    /// to log the message now.
    pub(crate) fn first(&self) -> bool {
        !self.0.swap(true, Ordering::Relaxed)
    }

    /// The level to log the message at: `warn` the first time, and `debug`
    /// after.
    pub(crate) fn level(&self) -> log::Level {
        if self.first() {
            log::Level::Warn
        } else {
            log::Level::Debug
        }
    }

    /// Whether the message has been logged.
    #[cfg(test)]
    pub(crate) fn logged(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::WarnOnce;

    #[test]
    fn a_message_is_logged_once_then_at_debug() {
        let once = WarnOnce::new();
        assert!(!once.logged());
        assert_eq!(once.level(), log::Level::Warn);
        assert!(once.logged());
        assert_eq!(once.level(), log::Level::Debug);
        assert!(!once.first());

        let once = WarnOnce::new();
        assert!(once.first());
        assert!(!once.first());
    }
}
