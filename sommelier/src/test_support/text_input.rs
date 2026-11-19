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
//! This is the only handler test code outside that file which touches the
//! text-input storage, the IME core's ([`crate::ime::ImeCore`]), so the
//! core's switch from the old handlers to `step` rewrites only this
//! module. A test that can pin what the peers receive uses
//! `proxy::harness::Session` instead.

use crate::ime::{Assembly, HostExt, HostTi, LinkState, SeatId, SurfaceId, TextInput, TiId};
use crate::state::{Context, HostId, InternalCallback, V3Pending};

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
    /// The guest surface its focus is on, if any; with none, the field is
    /// not enabled, but the host still active.
    pub(crate) focus: Option<u32>,
    /// The committed surrounding text: its text, cursor and anchor.
    pub(crate) surrounding_text: Option<(&'a str, i32, i32)>,
    /// The preedit the guest shows, with the cursor at its end.
    pub(crate) preedit: Option<&'a str>,
    /// The commits the guest has made.
    pub(crate) commits: u32,
    /// Whether state waits for its commit or for its host event: the
    /// guest's next enable, surrounding text and cursor rectangle, and the
    /// host's preedit cursor and preedit styling.
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
    let ext = active.ext.map(|id| HostExt {
        id,
        version: ctx.shadow_table.host_object_version_or_unknown(id),
        manager: ctx.host_text_input_extension_v1_id.unwrap_or_default(),
    });
    let host = HostTi {
        v1: active.host,
        ext,
    };
    let ti = TiId(guest);
    let focus = active.focus.map(SurfaceId);
    ctx.ime.insert(ti, SeatId(active.seat), host, focus);
    let t = ctx.ime.get_mut(ti).expect("just inserted");
    t.set_link_for_test(LinkState::Attached { content_type: None });
    if let Some(committed) = t.committed_mut_for_test() {
        committed.enabled = true;
        committed.surrounding_text = active
            .surrounding_text
            .map(|(text, cursor, anchor)| (text.to_string(), cursor, anchor));
    }
    if let Some(preedit) = active.preedit {
        t.show_for_test(preedit);
    }
    t.set_commits_for_test(active.commits);
    if active.pending {
        let surrounding_text = t
            .field()
            .map(|(_, committed)| committed.surrounding_text.clone());
        if let Some(pending) = t.pending_mut() {
            *pending = V3Pending {
                enabled: Some(true),
                surrounding_text,
                content_type: None,
                cursor_rect: Some((1, 2, 3, 4)),
            };
        }
        t.set_assembly_for_test(Assembly {
            cursor: Some(1),
            selection: Some((0, 1)),
        });
    }
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
    let t = ctx.ime.get(TiId(guest)).expect("a text input");
    let committed = t.field().map(|(_, committed)| committed);
    View {
        focus: t.focused_surface().map(|surface| surface.0),
        enabled: t.is_enabled(),
        surrounding_text: committed.and_then(|committed| committed.surrounding_text.clone()),
        host: match t.link() {
            LinkState::Detached => Host::Inactive,
            LinkState::Attached { .. } => Host::Active,
            LinkState::Draining { barrier } => Host::Draining(
                ctx.internal_callbacks
                    .awaits(InternalCallback::TextInput { token: barrier })
                    .expect("a drain waits for its callback"),
            ),
        },
        preedit: t.shown_for_test().map(str::to_string),
        commits: t.serial().get(),
        pending: *t.pending_for_test() != V3Pending::default()
            || t.assembly_for_test() != Assembly::default(),
    }
}

/// All the proxy holds for a text input, to check that something left it
/// untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot(TextInput);

/// All the proxy holds for text input `guest`.
pub(crate) fn snapshot(ctx: &Context, guest: u32) -> Snapshot {
    Snapshot(ctx.ime.get(TiId(guest)).expect("a text input").clone())
}
