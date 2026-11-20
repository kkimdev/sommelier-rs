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

use crate::handler::Handlers;
use crate::warn_once::WarnOnce;

use crate::handler::callback::{internal_sync, queue_internal_sync};
use crate::ime::{
    self, Edit, Env, HostExt, HostIme, HostOut, HostTi, Input, Piece, SeatId, SurfaceId, TiId,
};
use crate::keys::KeyInput;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::protocols::wayland::wl_seat;
use crate::state::{Context, InternalCallback, KeyboardFocusRegistry, Link, ShadowTable};
use crate::wire::{Action, IndexedString, Outgoing};

/// The facts the IME core reads and does not own: the object table's and
/// the keyboard focus's. It borrows only those, so that the core can step
/// beside it.
pub(crate) struct CtxEnv<'a> {
    table: &'a ShadowTable,
    focus: &'a KeyboardFocusRegistry,
}

impl<'a> CtxEnv<'a> {
    #[cfg(test)]
    pub(crate) fn of(ctx: &'a Context) -> Self {
        Self::new(&ctx.shadow_table, &ctx.keyboard_focus)
    }

    /// The facts of the context whose object table is `table` and whose
    /// keyboard focus is `focus`, borrowed apart from its IME core, which
    /// can then step beside them.
    pub(crate) fn new(table: &'a ShadowTable, focus: &'a KeyboardFocusRegistry) -> Self {
        Self { table, focus }
    }
}

impl Env for CtxEnv<'_> {
    fn host_surface(&self, surface: SurfaceId) -> Option<u32> {
        self.table.get_host_id(surface.0)
    }

    fn host_seat(&self, seat: SeatId) -> Option<u32> {
        live_host_seat(self.table, seat.0)
    }

    fn keyboard_focus(&self, seat: SeatId) -> Option<SurfaceId> {
        self.focus.surface_for_seat(seat.0).map(SurfaceId)
    }
}

/// Hand `input` to the IME core and apply its effects where the message
/// is handled (design 3.3): the guest events, the host requests and
/// barriers, then what it asks of the keyboard concern.
///
/// # Returns
///
/// Whether a press is to be forwarded ([`ime::Effects::forward`]).
pub(crate) fn ime(ctx: &mut Context, input: ime::Input) -> bool {
    // Not `CtxEnv::of`, which would borrow the whole context while its IME
    // core steps.
    let env = CtxEnv::new(&ctx.shadow_table, &ctx.keyboard_focus);
    let fx = ctx.ime.step(input, &env);
    ctx.host_to_client_queue.extend(fx.to_guest);
    for out in fx.to_host {
        match out {
            HostOut::Request(request) => ctx.client_to_host_queue.push(request),
            HostOut::Barrier(token) => {
                queue_internal_sync(ctx, InternalCallback::TextInput { token });
            }
            HostOut::Glued {
                request: (mut bytes, mut fds),
                barrier,
            } => {
                // One entry, which a submission never splits
                // (`connection::OutgoingMessages`).
                let token = barrier;
                let (_, (sync, sync_fds)) =
                    internal_sync(ctx, InternalCallback::TextInput { token });
                bytes.extend_from_slice(&sync);
                fds.extend(sync_fds);
                ctx.client_to_host_queue.push((bytes, fds));
            }
            HostOut::Destroy { request, id } => {
                ctx.client_to_host_queue.push(request);
                ctx.retire_destroyed_host_object(id);
            }
        }
    }
    // What the IME core asks of the keyboard core.
    for key in fx.keys {
        crate::handler::keyboard::keys(ctx, KeyInput::Ime(key));
    }
    fx.forward
}

impl zwp_text_input_manager_v1::ZwpTextInputManagerV1Handler for Handlers {}

impl zwp_text_input_v1::ZwpTextInputV1Handler for Handlers {
    /// H7. The v1 `commit` argument is the text to commit if this preedit
    /// is reset, which Exo always sends equal to `text`: it is not text to
    /// insert alongside the preedit.
    fn on_preedit_string(
        ctx: &mut Context,
        serial: u32,
        text: &IndexedString,
        commit: &String,
    ) -> Action {
        let edit = Edit::Preedit {
            serial,
            text: text.clone(),
            commit: commit.clone(),
        };
        host_v1_event(ctx, edit)
    }

    /// H8.
    fn on_commit_string(ctx: &mut Context, serial: u32, text: &String) -> Action {
        let text = text.clone();
        host_v1_event(ctx, Edit::Commit { serial, text })
    }

    /// H10: dropped. Exo sends no keysym for a physical key: its one
    /// `SendKey` caller, `TextInput::InsertChar`, sends a key the input
    /// method handled with no `DomCode` as `commit_string` instead, and
    /// never sends a release, so a keysym delivered as a key would stay
    /// pressed in the guest. The first is logged.
    fn on_keysym(
        _ctx: &mut Context,
        _serial: u32,
        _time: u32,
        sym: u32,
        state: u32,
        _modifiers: u32,
    ) -> Action {
        static KEYSYM: WarnOnce = WarnOnce::new();
        log_dropped_keysym(&KEYSYM, sym, state);
        Action::Drop
    }

    /// Exo attached the input method. While attached this answers the
    /// proxy's own `activate` ([`LinkState`]), and otherwise belongs to a
    /// link being drained, so it changes nothing.
    fn on_enter(ctx: &mut Context, surface: u32) -> Action {
        log::debug!(
            ">>> on_enter: host_v1_id={}, surface={surface}",
            ctx.last_sender_id
        );
        Action::Drop
    }

    /// H2: Exo detached the input method. While attached Exo did so on its
    /// own; a `leave` that answers the proxy's `deactivate` arrives while
    /// draining and is ignored (H2').
    fn on_leave(ctx: &mut Context) -> Action {
        let ti = v1_sender(ctx);
        // A recovery that waits reads the leave first (E6).
        crate::handler::keyboard::keys(ctx, KeyInput::TextInputLeave(ti));
        if let Some(ti) = ti {
            ime(ctx, Input::HostLeave(ti));
        }
        Action::Drop
    }

    fn on_modifiers_map(_ctx: &mut Context, _map: &[u8]) -> Action {
        log::trace!(">>> on_modifiers_map: len={}", _map.len());
        Action::Drop
    }

    fn on_input_panel_state(_ctx: &mut Context, _state: u32) -> Action {
        log::trace!(">>> on_input_panel_state: state={}", _state);
        Action::Drop
    }

    /// H4.
    fn on_preedit_styling(ctx: &mut Context, index: u32, length: u32, style: u32) -> Action {
        let piece = Piece::Styling {
            index,
            length,
            style,
        };
        host_v1_event(ctx, piece)
    }

    /// H5.
    fn on_preedit_cursor(ctx: &mut Context, index: i32) -> Action {
        host_v1_event(ctx, Piece::Cursor(index))
    }

    /// H6: Exo sends it only right before a commit_string, which reads it
    /// as a selection; text-input-v3 has no cursor-position event.
    fn on_cursor_position(ctx: &mut Context, index: i32, anchor: i32) -> Action {
        host_v1_event(ctx, Piece::CursorPosition { index, anchor })
    }

    /// H9. Exo counts `index` from the start of the surrounding text,
    /// against the v1 XML (docs/INVARIANTS.md, "Exo's deletes are absolute
    /// and stand alone").
    fn on_delete_surrounding_text(ctx: &mut Context, index: i32, length: u32) -> Action {
        host_v1_event(ctx, Edit::Delete { index, length })
    }

    fn on_language(ctx: &mut Context, serial: u32, _language: &String) -> Action {
        let host_id = ctx.last_sender_id;
        let guest_id = ctx.shadow_table.get_guest_id(host_id);
        log::trace!(
            ">>> on_language: host_id={}, guest_id={:?}, serial={}, language={:?}",
            host_id,
            guest_id,
            serial,
            _language
        );
        Action::Drop
    }

    fn on_text_direction(ctx: &mut Context, serial: u32, _direction: u32) -> Action {
        let host_id = ctx.last_sender_id;
        let guest_id = ctx.shadow_table.get_guest_id(host_id);
        log::trace!(
            ">>> on_text_direction: host_id={}, guest_id={:?}, serial={}, direction={}",
            host_id,
            guest_id,
            serial,
            _direction
        );
        Action::Drop
    }
}

/// The text input whose host `zwp_text_input_v1` sent the event being
/// handled. A host text input is paired with one guest text input until
/// the guest destroys it.
fn v1_sender(ctx: &Context) -> Option<TiId> {
    ctx.shadow_table.get_guest_id(ctx.last_sender_id).map(TiId)
}

/// H4 to H9: hand a host `zwp_text_input_v1` event to the IME core.
fn host_v1_event(ctx: &mut Context, event: impl Into<HostIme>) -> Action {
    if let Some(ti) = v1_sender(ctx) {
        ime(ctx, Input::V1(ti, event.into()));
    }
    Action::Drop
}

impl zcr_text_input_extension_v1::ZcrTextInputExtensionV1Handler for Handlers {}

impl zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler for Handlers {
    /// X1: Exo's cursor-relative region of its text becomes the preedit.
    fn on_set_preedit_region(ctx: &mut Context, index: i32, length: u32) -> Action {
        host_ext_event(ctx, Edit::PreeditRegion { index, length })
    }
    fn on_clear_grammar_fragments(_ctx: &mut Context, _start: u32, _end: u32) -> Action {
        log::trace!(
            ">>> on_clear_grammar_fragments: start={}, end={}",
            _start,
            _end
        );
        Action::Drop
    }
    fn on_add_grammar_fragment(
        _ctx: &mut Context,
        _start: u32,
        _end: u32,
        _suggestion: &String,
    ) -> Action {
        log::trace!(
            ">>> on_add_grammar_fragment: start={}, end={}, suggestion={:?}",
            _start,
            _end,
            _suggestion
        );
        Action::Drop
    }
    fn on_set_autocorrect_range(_ctx: &mut Context, _start: u32, _end: u32) -> Action {
        log::trace!(
            ">>> on_set_autocorrect_range: start={}, end={}",
            _start,
            _end
        );
        Action::Drop
    }
    fn on_set_virtual_keyboard_occluded_bounds(
        _ctx: &mut Context,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Action {
        log::trace!(
            ">>> on_set_virtual_keyboard_occluded_bounds: x={}, y={}, w={}, h={}",
            _x,
            _y,
            _width,
            _height
        );
        Action::Drop
    }
    /// X2, and X2' with no preedit shown.
    fn on_confirm_preedit(ctx: &mut Context, selection_behavior: u32) -> Action {
        let keep_selection = selection_behavior
            == zcr_extended_text_input_v1::CONFIRM_PREEDIT_SELECTION_BEHAVIOR_UNCHANGED;
        host_ext_event(ctx, Edit::Confirm { keep_selection })
    }
}

/// X1 and X2: hand a host `zcr_extended_text_input_v1` event to the IME
/// core, which knows each extension's text input.
fn host_ext_event(ctx: &mut Context, event: Edit) -> Action {
    let ext = ctx.last_sender_id;
    ime(ctx, Input::Ext { ext, event });
    Action::Drop
}

impl zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler for Handlers {
    fn on_destroy(_ctx: &mut Context) -> Action {
        // This manager is synthetic and has no host-side object. The
        // generated local-only destructor path queues its guest delete_id
        // after this handler returns.
        Action::Drop
    }

    fn on_get_text_input(ctx: &mut Context, id: u32, seat: u32) -> Action {
        // A new object takes its parent's version: the guest text input its
        // guest manager's, and each host object its host manager's.
        let guest_manager = ctx.last_sender_id;
        let Some(guest_manager_version) = ctx.shadow_table.guest_object_version(guest_manager)
        else {
            return ctx.fail(format_args!(
                "zwp_text_input_manager_v3 {guest_manager} has no version"
            ));
        };
        let managers = host_text_input_managers(ctx);
        let Some((host_manager_id, host_ext_manager_id)) = managers else {
            // The guest owns the ID, so its requests must still find an
            // object: one that has no input method.
            warn_no_input_method(&NO_INPUT_METHOD_WARNED);
            ctx.shadow_table
                .create_local(id, zwp_text_input_v3::NAME, guest_manager_version);
            return Action::Drop;
        };
        let versions = [host_manager_id, host_ext_manager_id]
            .map(|manager| ctx.shadow_table.host_object_version(manager));
        let [Some(host_manager_version), Some(host_ext_version)] = versions else {
            return ctx.fail(format_args!(
                "host text-input managers {host_manager_id} and {host_ext_manager_id} \
                 have versions {versions:?}"
            ));
        };
        let host_v1_id = ctx.shadow_table.create_bridged(
            id,
            zwp_text_input_v3::NAME,
            guest_manager_version,
            zwp_text_input_v1::NAME,
            host_manager_version,
        );
        let host_ext_id = ctx
            .shadow_table
            .create_host_only(zcr_extended_text_input_v1::NAME, host_ext_version);
        let create = Input::Create {
            ti: TiId(id),
            seat: SeatId(seat),
            host: HostTi {
                v1: host_v1_id,
                ext: HostExt {
                    id: host_ext_id,
                    manager: host_ext_manager_id,
                },
            },
            manager: host_manager_id,
        };
        ime(ctx, create);
        Action::Drop
    }
}

/// The host's `zwp_text_input_manager_v1` and `zcr_text_input_extension_v1`
/// that a text input is created from: `None` unless the host has both, the
/// extension at v11 or later, the first whose confirmations are
/// `confirm_preedit`, which the IME core's Exo mirror reads (`docs/IME.md`
/// E14).
fn host_text_input_managers(ctx: &Context) -> Option<(u32, u32)> {
    let manager = ctx.host_text_input_manager_v1_id?;
    let ext_manager = ctx.host_text_input_extension_v1_id?;
    ctx.shadow_table
        .host_object_version(ext_manager)
        .is_some_and(|version| version >= zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT_SINCE)
        .then_some((manager, ext_manager))
}

/// Log, once per `once` (the process's, or a test's own), that a
/// `zwp_text_input_v1.keysym` was dropped: at the default level, so a host
/// that does send keysyms shows it.
///
/// # Returns
///
/// Whether this call logged.
fn log_dropped_keysym(once: &WarnOnce, sym: u32, state: u32) -> bool {
    if !once.first() {
        return false;
    }
    log::info!(
        "Dropping zwp_text_input_v1.keysym {sym:#x} (state {state}), which Exo is not known to \
         send; later ones are dropped unlogged"
    );
    true
}

/// Whether [`warn_no_input_method`] has warned. Every client connection
/// binds the same host globals, so one warning per process says it all.
static NO_INPUT_METHOD_WARNED: WarnOnce = WarnOnce::new();

/// Warn, once per `warned`, that text inputs get no input method.
///
/// # Returns
///
/// Whether this call logged the warning.
fn warn_no_input_method(warned: &WarnOnce) -> bool {
    if !warned.first() {
        return false;
    }
    log::warn!(
        "Text inputs get no input method: the host lacks zwp_text_input_manager_v1, or \
         zcr_text_input_extension_v1 v{} or later",
        zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT_SINCE
    );
    true
}

/// The host seat that can carry an `activate` or `deactivate` of a text
/// input of guest seat `guest_seat`: the host object of that seat, while
/// its ID names a live `wl_seat`.
///
/// A seat can end while its text inputs live on, and the guest may then
/// reuse its ID for any object, which the host would reject as a seat. A
/// seat awaiting its `delete_id` carries nothing either, and neither does
/// an ID with no host object (ID 0 is a protocol error).
fn live_host_seat(table: &ShadowTable, guest_seat: u32) -> Option<u32> {
    let seat = table.request_object(guest_seat, wl_seat::NAME)?;
    match seat {
        Link::Host(host_seat) => Some(host_seat),
        // The proxy implements no wl_seat itself.
        Link::Local => None,
    }
}

/// Forget guest `zwp_text_input_v3` `guest_id`, which ended (see
/// `crate::state::lifecycle`): the IME core deactivates it and destroys
/// its host extension (G10). Nothing goes to its v1 object once the host
/// has deleted it (`host_deleted`, F6).
pub(crate) fn forget_text_input(ctx: &mut Context, guest_id: u32, host_deleted: bool) {
    let ti = TiId(guest_id);
    crate::handler::keyboard::keys(ctx, KeyInput::TextInputEnded(ti));
    ime(ctx, Input::Destroy { ti, host_deleted });
}

/// A press on guest surface `surface`, a pointer button's or a
/// touch-down, that the guest has yet to get: end the composition of the
/// text input focused on it whose composition is current and shows a
/// preedit. Only one can be: one text input on a connection is enabled at
/// a time. The guest commits the preedit it shows, and the host `reset`s,
/// which drops Exo's composition without an event back (F4 of the IME
/// core).
///
/// # Returns
///
/// How to dispatch the press. If a composition ended, the press is re-sent
/// after the guest events that end it, given as `resend`, since a forward
/// would precede them, and the original is dropped; otherwise it is
/// forwarded.
pub(crate) fn end_composition_on_press(
    ctx: &mut Context,
    surface: u32,
    resend: Outgoing,
) -> Action {
    let press = Input::Press {
        surface: SurfaceId(surface),
        resend,
    };
    match ime(ctx, press) {
        true => Action::Forward,
        false => Action::Drop,
    }
}

/// The guest text input that sent the request being handled.
fn sender(ctx: &Context) -> TiId {
    TiId(ctx.last_sender_id)
}

/// Hand a guest request to the IME core (G3 to G9), which consumes it.
fn guest_request(ctx: &mut Context, input: Input) -> Action {
    ime(ctx, input);
    Action::Drop
}

impl zwp_text_input_v3::ZwpTextInputV3Handler for Handlers {
    fn on_destroy(ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        // A text input with no input method is local, and its destructor
        // completes in the proxy.
        if ctx.shadow_table.is_local(guest_id) {
            return Action::Drop;
        }
        // The v3 object is guest-facing but backed by a v1 host object whose
        // protocol has no destructor, so the host object stays for the
        // connection and the guest lifecycle completes locally. Ending the
        // object forgets its state (see `forget_text_input`).
        if ctx.guest_ended_host_kept(guest_id) {
            ctx.queue_delete_id(guest_id);
        } else {
            log::debug!(
                "Text input {} had already been retired before its destroy handler ran",
                guest_id
            );
        }
        Action::Drop
    }

    fn on_enable(ctx: &mut Context) -> Action {
        guest_request(ctx, Input::Enable(sender(ctx)))
    }

    fn on_disable(ctx: &mut Context) -> Action {
        guest_request(ctx, Input::Disable(sender(ctx)))
    }

    fn on_set_surrounding_text(
        ctx: &mut Context,
        text: &IndexedString,
        cursor: i32,
        anchor: i32,
    ) -> Action {
        let input = Input::SurroundingText(sender(ctx), text.clone(), cursor, anchor);
        guest_request(ctx, input)
    }

    /// The host's text-input-v1 has no text-change cause to forward it to.
    fn on_set_text_change_cause(_ctx: &mut Context, _cause: u32) -> Action {
        Action::Drop
    }

    fn on_set_content_type(ctx: &mut Context, hint: u32, purpose: u32) -> Action {
        guest_request(ctx, Input::ContentType(sender(ctx), (hint, purpose)))
    }

    fn on_set_cursor_rectangle(
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let input = Input::CursorRect(sender(ctx), (x, y, width, height));
        guest_request(ctx, input)
    }

    fn on_commit(ctx: &mut Context) -> Action {
        guest_request(ctx, Input::Commit(sender(ctx)))
    }
}

#[cfg(test)]
pub(super) mod tests;
