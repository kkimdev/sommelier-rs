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

//! The guest commit count of a text input, and the serial every `done`
//! carries (B4 in the design).

/// The number of `commit` requests the guest made on one text input,
/// counted whether or not the commit reached the host (INVARIANTS "Every
/// v3 commit counts"). Its field is private, so [`Self::bump`] is its only
/// writer.
#[derive(Debug, Default)]
pub(super) struct CommitCount(u32);

impl CommitCount {
    /// Count one guest `commit`. Only the Commit arm of `step` calls it,
    /// through `TextInput::count_commit`.
    pub(super) fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// The serial of the next `done`: the commits counted so far.
    pub(super) fn serial(&self) -> Serial {
        Serial(self.0)
    }
}

/// The serial of a text-input-v3 `done`. Only [`CommitCount::serial`]
/// makes one, so a `done` cannot carry another number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Serial(u32);

impl Serial {
    pub(super) fn get(self) -> u32 {
        self.0
    }
}
