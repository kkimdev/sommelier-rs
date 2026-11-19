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

//! The proxy's text-input state, for handler tests outside
//! `handler/text_input.rs` that set up or read state no peer can observe.
//!
//! This is the only test code outside that file which touches the
//! text-input storage, so the IME core's storage switch
//! (docs/IME_REDESIGN.md, C3a) rewrites only this module. A test that can
//! pin what the peers receive uses `proxy::harness::Session` instead.

use crate::state::{Context, HostApplied, HostId, ShownPreedit, TextInputState, V3Pending};

/// A text input the guest enabled and committed, and the host activated,
/// as [`insert`] sets it up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Active<'a> {
    /// The host `zwp_text_input_v1` behind it.
    pub(crate) host: u32,
    /// The host `zcr_extended_text_input_v1` behind it, if any.
    pub(crate) ext: Option<u32>,
    /// The guest `wl_seat` it was created for.
    pub(crate) seat: u32,
    /// The guest surface its focus is on, if any.
    pub(crate) focus: Option<u32>,
    /// The committed surrounding text: its text, cursor and anchor.
    pub(crate) surrounding_text: Option<(&'a str, i32, i32)>,
    /// The preedit the guest shows, with the cursor at its end.
    pub(crate) preedit: Option<&'a str>,
    /// The commits the guest has made.
    pub(crate) commits: u32,
    /// Whether state waits for its commit or for its host event: the
    /// guest's next enable, surrounding text and cursor rectangle, and the
    /// host's preedit cursor, preedit styling and cursor position.
    pub(crate) pending: bool,
}

impl Active<'_> {
    /// A text input of `seat` focused on `focus`, whose host is `host`,
    /// after one commit, with nothing else.
    pub(crate) const fn new(host: u32, seat: u32, focus: Option<u32>) -> Self {
        Self {
            host,
            ext: None,
            seat,
            focus,
            surrounding_text: None,
            preedit: None,
            commits: 1,
            pending: false,
        }
    }
}

/// Give guest text input `guest`, whose objects the test has paired, the
/// state `active` describes.
pub(crate) fn insert(ctx: &mut Context, guest: u32, active: Active) {
    let mut state = TextInputState::new(active.host, active.ext, active.seat, active.focus);
    state.committed.enabled = true;
    state.committed.surrounding_text = active
        .surrounding_text
        .map(|(text, cursor, anchor)| (text.to_string(), cursor, anchor));
    if let Some(preedit) = active.preedit {
        state.current_preedit = ShownPreedit::at_end(preedit);
    }
    state.set_guest_commits(active.commits);
    if active.pending {
        state.pending = V3Pending {
            enabled: Some(true),
            surrounding_text: Some(state.committed.surrounding_text.clone()),
            content_type: None,
            cursor_rect: Some((1, 2, 3, 4)),
        };
        state.pending_preedit_cursor = Some(1);
        state.pending_preedit_selection = Some((0, 1));
        state.pending_cursor_position = Some((1, 1));
    }
    state.host = HostApplied::Active { content_type: None };
    ctx.text_inputs.insert(guest, state);
}

/// What the host text input has applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    /// No input method is attached and no events are in flight.
    Inactive,
    /// The input method is attached.
    Active,
    /// A deactivate detached it, and the host callback drains the events
    /// sent before it.
    Draining(HostId),
}

/// What the proxy holds for a text input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct View {
    /// The guest surface its focus is on, if any.
    pub(crate) focus: Option<u32>,
    /// Whether the committed field is enabled.
    pub(crate) enabled: bool,
    /// The committed surrounding text, cursor and anchor.
    pub(crate) surrounding_text: Option<(String, i32, i32)>,
    pub(crate) host: Host,
    /// The preedit the guest shows, if any.
    pub(crate) preedit: Option<String>,
    /// The commits the guest has made.
    pub(crate) commits: u32,
    /// Whether any state waits for its commit or its host event, as
    /// [`Active::pending`] lists.
    pub(crate) pending: bool,
}

/// What the proxy holds for text input `guest`.
pub(crate) fn view(ctx: &Context, guest: u32) -> View {
    let state = &ctx.text_inputs[&guest];
    View {
        focus: state.active_surface,
        enabled: state.committed.enabled,
        surrounding_text: state.committed.surrounding_text.clone(),
        host: match state.host {
            HostApplied::Inactive => Host::Inactive,
            HostApplied::Active { .. } => Host::Active,
            HostApplied::Draining { callback } => Host::Draining(callback),
        },
        preedit: state
            .current_preedit
            .has_text()
            .then(|| state.current_preedit.text.clone()),
        commits: state.guest_commit_serial(),
        pending: state.pending != V3Pending::default()
            || state.pending_preedit_cursor.is_some()
            || state.pending_preedit_selection.is_some()
            || state.pending_cursor_position.is_some(),
    }
}

/// All the proxy holds for a text input, to check that something left it
/// untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot(TextInputState);

/// All the proxy holds for text input `guest`.
pub(crate) fn snapshot(ctx: &Context, guest: u32) -> Snapshot {
    Snapshot(ctx.text_inputs[&guest].clone())
}
