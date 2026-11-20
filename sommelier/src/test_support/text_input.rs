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
//! text-input storage, the IME core's ([`crate::ime::ImeCore`]), so a
//! change of that storage rewrites only this module. A test that can pin
//! what the peers receive uses `proxy::harness::Session` instead.

use crate::handler::text_input::CtxEnv;
use crate::ime::{HostExt, HostTi, LinkState, SeatId, SurfaceId, TextInput, TiId, V3Pending};
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::Iface;
use crate::state::{Context, GuestId, HostId, InternalCallback};

/// A text input the guest committed, enabled unless [`Active::enabled`]
/// says otherwise, and the host activated if enabled, as [`insert`] sets it
/// up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Active<'a> {
    /// The host `zwp_text_input_v1` behind it.
    pub(crate) host: HostId,
    /// The host `zcr_extended_text_input_v1` behind it, if the test names
    /// one; otherwise [`insert`] creates one.
    pub(crate) ext: Option<HostId>,
    /// The guest `wl_seat` it was created for.
    pub(crate) seat: GuestId,
    /// The guest surface its focus is on, if any; with none, the field is
    /// not enabled and the host not active, as no text input can be
    /// active unless enabled and focused (the IME core's I1a).
    pub(crate) focus: Option<GuestId>,
    /// Whether the guest enabled the focused field, which the host then
    /// activated. One text input on a connection is enabled at a time.
    pub(crate) enabled: bool,
    /// The committed surrounding text: its text, cursor and anchor.
    pub(crate) surrounding_text: Option<(&'a str, i32, i32)>,
    /// The preedit the guest shows, with the cursor at its end.
    pub(crate) preedit: Option<&'a str>,
    /// The commits the guest has made.
    pub(crate) commits: u32,
    /// Whether state waits for its commit: the guest's next enable,
    /// surrounding text and cursor rectangle.
    pub(crate) pending: bool,
}

impl Active<'_> {
    /// A text input of `seat` focused on `focus`, whose host is `host`,
    /// enabled with one commit if focused, with nothing else.
    pub(crate) const fn new(host: HostId, seat: GuestId, focus: Option<GuestId>) -> Self {
        Self {
            host,
            ext: None,
            seat,
            focus,
            enabled: true,
            surrounding_text: None,
            preedit: None,
            commits: 1,
            pending: false,
        }
    }
}

/// Give guest text input `guest`, whose objects the test has paired, the
/// state `active` describes. A focused one is attached with the content
/// type its field calls for, as the reconcile of the guest commit that set
/// its state would, so its surface and seat must be live. The state must
/// be one the IME core can reach, which its invariants check: one text
/// input on the connection is enabled at a time.
pub(crate) fn insert(ctx: &mut Context, guest: GuestId, active: Active) {
    let id = active.ext.unwrap_or_else(|| {
        let version = zcr_extended_text_input_v1::VERSION;
        ctx.shadow_table
            .create_host_only(Iface::ZcrExtendedTextInputV1, version)
    });
    let ext = HostExt {
        id,
        // A context without the host's extension names none, as no
        // request of the test's goes to it.
        manager: ctx
            .host_text_input_extension_v1_id
            .unwrap_or(HostId::for_test(0)),
    };
    let host = HostTi {
        v1: active.host,
        ext,
    };
    let ti = TiId(guest);
    let focus = active.focus.map(SurfaceId);
    ctx.ime
        .insert_for_test(ti, SeatId(active.seat), host, focus);
    let env = CtxEnv::new(&ctx.shadow_table, &ctx.keyboard_focus);
    let t = ctx.ime.get_mut(ti).expect("just inserted");
    if let Some(committed) = t.committed_mut_for_test() {
        committed.enabled = active.enabled;
        committed.surrounding_text_support = active.surrounding_text.is_some();
        if active.enabled {
            t.attach_for_test(&env);
        }
    }
    if let Some(preedit) = active.preedit {
        t.show_for_test(preedit);
    }
    t.set_commits_for_test(active.commits);
    if active.pending {
        // The guest states its committed text again.
        let surrounding_text = t.field().map(|_| {
            active
                .surrounding_text
                .map(|(text, cursor, anchor)| (text.to_string(), cursor, anchor))
        });
        if let Some(pending) = t.pending_mut() {
            *pending = V3Pending {
                enabled: Some(true),
                surrounding_text,
                content_type: None,
                cursor_rect: Some((1, 2, 3, 4)),
            };
        }
    }
    ctx.ime
        .assert_invariants(&CtxEnv::new(&ctx.shadow_table, &ctx.keyboard_focus));
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
    pub(crate) focus: Option<GuestId>,
    /// Whether the committed field is enabled.
    pub(crate) enabled: bool,
    /// Whether the committed field states its surrounding text.
    pub(crate) surrounding_text_support: bool,
    pub(crate) host: Host,
    /// The preedit the guest shows, if any.
    pub(crate) preedit: Option<String>,
    /// The commits the guest has made.
    pub(crate) commits: u32,
    /// Whether any state waits for its commit, as [`Active::pending`]
    /// lists.
    pub(crate) pending: bool,
}

/// What the proxy holds for text input `guest`.
pub(crate) fn view(ctx: &Context, guest: GuestId) -> View {
    let t = ctx.ime.get(TiId(guest)).expect("a text input");
    let committed = t.field().map(|(_, committed)| committed);
    View {
        focus: t.focused_surface().map(|surface| surface.0),
        enabled: t.is_enabled(),
        surrounding_text_support: committed
            .is_some_and(|committed| committed.surrounding_text_support),
        host: match t.link() {
            LinkState::Detached => Host::Inactive,
            LinkState::Attached { .. } => Host::Active,
            LinkState::Draining { barrier } => Host::Draining(
                ctx.internal_callbacks
                    .awaits(InternalCallback::TextInput { token: *barrier })
                    .expect("a drain waits for its callback"),
            ),
        },
        preedit: t.shown_for_test().map(str::to_string),
        commits: t.serial().get(),
        pending: *t.pending_for_test() != V3Pending::default(),
    }
}

/// All the proxy holds for a text input, to check that something left it
/// untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot(TextInput);

/// All the proxy holds for text input `guest`.
pub(crate) fn snapshot(ctx: &Context, guest: GuestId) -> Snapshot {
    Snapshot(ctx.ime.get(TiId(guest)).expect("a text input").clone())
}
