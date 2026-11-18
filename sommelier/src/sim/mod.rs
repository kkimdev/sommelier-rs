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

//! The deterministic simulation test of the IME bridge.
//!
//! It types into simulated clients through the real proxy and checks that
//! they end with the text the user intended. So far it has the pieces that
//! define that text: a Hangul composer and the reference editor.

mod client;
mod clock;
mod display;
mod editor;
mod engine;
pub(crate) mod exo;
mod hangul;
mod keymap;
pub(crate) mod oracle;
mod sched;
mod user;
