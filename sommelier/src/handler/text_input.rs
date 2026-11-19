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

use crate::handler::callback::queue_internal_sync;
use crate::handler::ime_keys::{
    end_backspace_repeat_for_seat, end_backspace_repeat_for_text_input, forget_held_keys_for_seat,
    forget_recovery_of_text_input, held_backspace_keyboard_for_seat, hold_recovery,
};
use crate::ime::{
    self, Env, HostExt, HostIme, HostOut, HostTi, Input, KeyEffect, LeaseOutcome, SeatId,
    SurfaceId, TiId,
};
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::protocols::wayland::wl_seat;
#[cfg(test)]
use crate::state::GuestKeyOwner;
use crate::state::{
    Context, InternalCallback, KeyboardFocusRegistry, Link, SeatFocusChange, ShadowTable,
};
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
            HostOut::Retire(id) => ctx.retire_destroyed_host_object(id),
        }
    }
    for key in fx.keys {
        apply_key_effect(ctx, key);
    }
    fx.forward
}

/// Apply what the IME core asks of the keyboard concern.
fn apply_key_effect(ctx: &mut Context, effect: KeyEffect) {
    match effect {
        KeyEffect::Lease { .. } => apply_lease(ctx, effect),
        KeyEffect::RecoverHeldKey { ti, seat } => hold_recovery(ctx, ti, seat),
    }
}

/// The guest text input of host text input `host_id`, and its guest seat,
/// while its host keysyms reach the guest (H10).
pub(super) fn active_guest_for_host_text_input(ctx: &Context, host_id: u32) -> Option<(u32, u32)> {
    let guest_id = ctx.shadow_table.get_guest_id(host_id)?;
    let seat = ctx.ime.keysym_target(TiId(guest_id))?;
    Some((guest_id, seat.0))
}

/// Apply what the IME core asks of the Backspace repeat leased to a text
/// input.
fn apply_lease(ctx: &mut Context, effect: KeyEffect) {
    let KeyEffect::Lease { ti, seat, outcome } = effect else {
        unreachable!("a transaction ends with a lease");
    };
    let held = match outcome {
        LeaseOutcome::End => None,
        LeaseOutcome::ArmIfBackspaceHeld => held_backspace_keyboard_for_seat(ctx, seat.0),
    };
    match held {
        Some(host_keyboard_id) => {
            let armed = ctx.key_generations.arm_ime_repeat(
                host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
                ti.0,
            );
            debug_assert!(armed, "held Backspace disappeared within one event");
        }
        None => end_backspace_repeat_for_text_input(ctx, seat.0, ti.0),
    }
}

pub struct TextInputManagerV1Handler;
impl zwp_text_input_manager_v1::ZwpTextInputManagerV1Handler for TextInputManagerV1Handler {}

pub struct TextInputV1Handler;
impl zwp_text_input_v1::ZwpTextInputV1Handler for TextInputV1Handler {
    /// H7. The v1 `commit` argument is the text to commit if this preedit
    /// is reset, not text to insert alongside every live preedit: Exo
    /// commonly sends the same non-empty `text` and `commit` while
    /// composing Korean, and forwarding both would insert every
    /// intermediate syllable. The core commits it only when the host
    /// clears the preedit.
    fn on_preedit_string(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        text: &IndexedString,
        commit: &String,
    ) -> Action {
        let event = HostIme::Preedit {
            serial,
            text: text.clone(),
            commit: commit.clone(),
        };
        host_v1_event(ctx, event)
    }

    /// H8.
    fn on_commit_string(&mut self, ctx: &mut Context, serial: u32, text: &String) -> Action {
        let text = text.clone();
        host_v1_event(ctx, HostIme::Commit { serial, text })
    }

    fn on_keysym(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        time: u32,
        sym: u32,
        state: u32,
        _modifiers: u32,
    ) -> Action {
        crate::handler::ime_keys::on_keysym(ctx, serial, time, sym, state)
    }

    /// Exo attached the input method. While attached this answers the
    /// proxy's own `activate` ([`LinkState`]), and otherwise belongs to a
    /// link being drained, so it changes nothing.
    fn on_enter(&mut self, ctx: &mut Context, surface: u32) -> Action {
        log::debug!(
            ">>> on_enter: host_v1_id={}, surface={surface}",
            ctx.last_sender_id
        );
        Action::Drop
    }

    /// H2: Exo detached the input method. While attached Exo did so on its
    /// own; a `leave` that answers the proxy's `deactivate` arrives while
    /// draining and is ignored (H2').
    fn on_leave(&mut self, ctx: &mut Context) -> Action {
        if let Some(ti) = v1_sender(ctx) {
            let detached = ctx.ime.keysym_target(ti);
            ime(ctx, Input::HostLeave(ti));
            if let Some(seat) = detached {
                forget_held_keys_for_seat(ctx, seat.0);
            }
        }
        Action::Drop
    }

    fn on_modifiers_map(&mut self, _ctx: &mut Context, _map: &[u8]) -> Action {
        log::trace!(">>> on_modifiers_map: len={}", _map.len());
        Action::Drop
    }

    fn on_input_panel_state(&mut self, _ctx: &mut Context, _state: u32) -> Action {
        log::trace!(">>> on_input_panel_state: state={}", _state);
        Action::Drop
    }

    /// H4.
    fn on_preedit_styling(
        &mut self,
        ctx: &mut Context,
        index: u32,
        length: u32,
        style: u32,
    ) -> Action {
        host_v1_event(
            ctx,
            HostIme::PreeditStyling {
                index,
                length,
                style,
            },
        )
    }

    /// H5.
    fn on_preedit_cursor(&mut self, ctx: &mut Context, index: i32) -> Action {
        host_v1_event(ctx, HostIme::PreeditCursor(index))
    }

    /// H6: Exo sends it only right before a commit_string, and
    /// text-input-v3 has no cursor-position event.
    fn on_cursor_position(&mut self, ctx: &mut Context, index: i32, anchor: i32) -> Action {
        host_v1_event(ctx, HostIme::CursorPosition { index, anchor })
    }

    /// H9. Exo counts `index` from the start of the surrounding text,
    /// against the v1 XML (docs/INVARIANTS.md, "Exo's deletes are absolute
    /// and stand alone").
    fn on_delete_surrounding_text(&mut self, ctx: &mut Context, index: i32, length: u32) -> Action {
        host_v1_event(ctx, HostIme::Delete { index, length })
    }

    fn on_language(&mut self, ctx: &mut Context, serial: u32, _language: &String) -> Action {
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

    fn on_text_direction(&mut self, ctx: &mut Context, serial: u32, _direction: u32) -> Action {
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
fn host_v1_event(ctx: &mut Context, event: HostIme) -> Action {
    if let Some(ti) = v1_sender(ctx) {
        ime(ctx, Input::V1(ti, event));
    }
    Action::Drop
}

pub struct TextInputExtensionV1Handler;
impl zcr_text_input_extension_v1::ZcrTextInputExtensionV1Handler for TextInputExtensionV1Handler {}

pub struct ExtendedTextInputV1Handler;
impl zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler for ExtendedTextInputV1Handler {
    /// X1: Exo's cursor-relative region of its text becomes the preedit.
    fn on_set_preedit_region(&mut self, ctx: &mut Context, index: i32, length: u32) -> Action {
        host_ext_event(ctx, HostIme::PreeditRegion { index, length })
    }
    fn on_clear_grammar_fragments(&mut self, _ctx: &mut Context, _start: u32, _end: u32) -> Action {
        log::trace!(
            ">>> on_clear_grammar_fragments: start={}, end={}",
            _start,
            _end
        );
        Action::Drop
    }
    fn on_add_grammar_fragment(
        &mut self,
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
    fn on_set_autocorrect_range(&mut self, _ctx: &mut Context, _start: u32, _end: u32) -> Action {
        log::trace!(
            ">>> on_set_autocorrect_range: start={}, end={}",
            _start,
            _end
        );
        Action::Drop
    }
    fn on_set_virtual_keyboard_occluded_bounds(
        &mut self,
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
    fn on_confirm_preedit(&mut self, ctx: &mut Context, _selection_behavior: u32) -> Action {
        host_ext_event(ctx, HostIme::Confirm)
    }
}

/// X1 and X2: hand a host `zcr_extended_text_input_v1` event to the IME
/// core, which knows each extension's text input.
fn host_ext_event(ctx: &mut Context, event: HostIme) -> Action {
    let ext = ctx.last_sender_id;
    ime(ctx, Input::Ext { ext, event });
    Action::Drop
}

pub struct TextInputManagerV3Handler;
impl zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler for TextInputManagerV3Handler {
    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        // This manager is synthetic and has no host-side object. The
        // generated local-only destructor path queues its guest delete_id
        // after this handler returns.
        Action::Drop
    }

    fn on_get_text_input(&mut self, ctx: &mut Context, id: u32, seat: u32) -> Action {
        let Some(host_manager_id) = ctx.host_text_input_manager_v1_id else {
            log::error!("Cannot create v3 text input without a host v1 manager");
            return Action::Drop;
        };
        // A new object takes its parent's version: the guest text input its
        // guest manager's, and each host object its host manager's.
        let guest_manager_version = ctx
            .shadow_table
            .guest_object_version_or_unknown(ctx.last_sender_id);
        let host_manager_version = ctx
            .shadow_table
            .host_object_version_or_unknown(host_manager_id);
        let host_v1_id = ctx.shadow_table.create_bridged(
            id,
            zwp_text_input_v3::NAME,
            guest_manager_version,
            zwp_text_input_v1::NAME,
            host_manager_version,
        );
        let host_ext = ctx
            .host_text_input_extension_v1_id
            .map(|host_ext_manager_id| {
                let host_ext_version = ctx
                    .shadow_table
                    .host_object_version_or_unknown(host_ext_manager_id);
                let host_ext_id = ctx
                    .shadow_table
                    .create_host_only(zcr_extended_text_input_v1::NAME, host_ext_version);
                HostExt {
                    id: host_ext_id,
                    version: host_ext_version,
                    manager: host_ext_manager_id,
                }
            });
        let create = Input::Create {
            ti: TiId(id),
            seat: SeatId(seat),
            host: HostTi {
                v1: host_v1_id,
                ext: host_ext,
            },
            manager: host_manager_id,
        };
        ime(ctx, create);
        Action::Drop
    }
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

/// Tell the IME core of each seat's keyboard focus change (F1), after the
/// keyboard concern's own teardown: the seat's peek watermarks and its
/// Backspace repeats end. `keyboard` is the guest's copy of the keyboard
/// events that moved the focus, which the guest gets after the commit of a
/// preedit the move ends, and before the text inputs' `leave` and `enter`,
/// so their sender handler drops the originals. They go with the first
/// change, the only one, as a keyboard belongs to one seat.
pub(crate) fn apply_keyboard_focus_changes(
    ctx: &mut Context,
    changes: &[SeatFocusChange],
    mut keyboard: Vec<Outgoing>,
) {
    // Keyboard events come from one keyboard, which belongs to one seat,
    // so at most one seat's focus changes with them.
    debug_assert!(
        keyboard.is_empty()
            || changes
                .iter()
                .filter(|c| c.previous_surface != c.current_surface)
                .count()
                <= 1,
        "keyboard events moved the focus of several seats: {changes:?}"
    );
    for change in changes {
        if change.previous_surface == change.current_surface {
            continue;
        }
        ctx.key_generations
            .clear_peek_watermarks_for_seat(change.guest_seat);
        end_backspace_repeat_for_seat(ctx, change.guest_seat);
        let focus = Input::FocusChanged {
            seat: SeatId(change.guest_seat),
            surface: change.current_surface.map(SurfaceId),
            keyboard: std::mem::take(&mut keyboard),
        };
        ime(ctx, focus);
    }
    ctx.host_to_client_queue.extend(keyboard);
}

/// Forget guest `zwp_text_input_v3` `guest_id`, which ended (see
/// `crate::state::lifecycle`): the IME core deactivates it and destroys
/// its host extension (G10). Nothing goes to its v1 object once the host
/// has deleted it (`host_deleted`, F6).
pub(crate) fn forget_text_input(ctx: &mut Context, guest_id: u32, host_deleted: bool) {
    let ti = TiId(guest_id);
    forget_recovery_of_text_input(ctx, ti);
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

pub struct TextInputV3Handler;
impl zwp_text_input_v3::ZwpTextInputV3Handler for TextInputV3Handler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        // The v3 object is guest-facing but backed by a v1 host object whose
        // protocol has no destructor, so the host object stays for the
        // connection and the guest lifecycle completes locally. Ending the
        // object forgets its state (see `forget_text_input`).
        if ctx.guest_ended_host_kept(guest_id) {
            crate::handler::display::queue_delete_id(ctx, guest_id);
        } else {
            log::debug!(
                "Text input {} had already been retired before its destroy handler ran",
                guest_id
            );
        }
        Action::Drop
    }

    fn on_enable(&mut self, ctx: &mut Context) -> Action {
        guest_request(ctx, Input::Enable(sender(ctx)))
    }

    fn on_disable(&mut self, ctx: &mut Context) -> Action {
        guest_request(ctx, Input::Disable(sender(ctx)))
    }

    fn on_set_surrounding_text(
        &mut self,
        ctx: &mut Context,
        text: &IndexedString,
        cursor: i32,
        anchor: i32,
    ) -> Action {
        let input = Input::SurroundingText(sender(ctx), text.clone(), cursor, anchor);
        guest_request(ctx, input)
    }

    /// The host's text-input-v1 has no text-change cause to forward it to.
    fn on_set_text_change_cause(&mut self, _ctx: &mut Context, _cause: u32) -> Action {
        Action::Drop
    }

    fn on_set_content_type(&mut self, ctx: &mut Context, hint: u32, purpose: u32) -> Action {
        guest_request(ctx, Input::ContentType(sender(ctx), (hint, purpose)))
    }

    fn on_set_cursor_rectangle(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let input = Input::CursorRect(sender(ctx), (x, y, width, height));
        guest_request(ctx, input)
    }

    fn on_commit(&mut self, ctx: &mut Context) -> Action {
        guest_request(ctx, Input::Commit(sender(ctx)))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::handler::callback::CallbackHandler;
    use crate::handler::ime_keys::backspace_repeat_active_for_keyboard;
    use crate::handler::keyboard::KeyboardHandler;
    use crate::ime::{delete_lengths, map_v3_content_type, LinkState};
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler;
    use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::ZwpTextInputV1Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::ZwpTextInputV3Handler;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_display;
    use crate::protocols::wayland::wl_keyboard::{self, WlKeyboardHandler};
    use crate::protocols::wayland::{
        wl_callback, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_touch,
    };
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session};
    use crate::state::HostId;
    use crate::test_support::{decoded, respell};
    use crate::wire::WL_DISPLAY_ID;
    use std::os::fd::OwnedFd;
    use zcr_extended_text_input_v1::{evt as ext, Request as Ext};
    use zcr_keyboard_extension_v1::Request as KeyboardExtension;
    use zcr_text_input_extension_v1::Request as ExtManager;
    use zwp_text_input_manager_v1::Request as V1Manager;
    use zwp_text_input_v1::{evt as v1, Request as V1};
    use zwp_text_input_v3::{req as v3, Event as V3};

    /// A guest `zwp_text_input_v3` created for a seat whose keyboard has
    /// given its focus to a surface, through a whole proxy [`Session`].
    struct Field {
        s: Session,
        /// The guest text input.
        ti: u32,
        /// The host `zwp_text_input_v1` behind it.
        host: u32,
        /// The host `zcr_extended_text_input_v1` behind it, 0 without one.
        host_ext: u32,
        seat: Object,
        keyboard: Object,
        surface: Object,
        /// The guest's `zwp_text_input_manager_v3`.
        manager: u32,
        /// The host `zcr_extended_keyboard_v1` of the keyboard, 0 without
        /// one.
        extended_keyboard: u32,
    }

    /// The text-input extension version the proxy binds.
    const EXTENSION: Option<u32> = Some(zcr_text_input_extension_v1::VERSION);

    /// What the host offers a [`Field`] besides its seat, keyboard and
    /// text-input manager.
    #[derive(Clone, Copy)]
    struct Host {
        /// The text-input extension's version, if the host has one.
        text_input_extension: Option<u32>,
        /// Whether the host advertises the keyboard extension, which
        /// reports each key before the input method takes it (`peek_key`)
        /// and takes the proxy's `ack_key`, and sends the keyboard the
        /// default keymap, whose keys repeat.
        keyboard_extension_with_keymap: bool,
    }

    /// A guest text input and the host objects behind it, which a
    /// [`Field`] drives one at a time.
    #[derive(Clone, Copy)]
    struct TextInput {
        ti: u32,
        host: u32,
        host_ext: u32,
    }

    /// What a commit that enables or disables a field sends the host
    /// besides its edits ([`Field::reset`]).
    #[derive(Default)]
    struct Reset {
        /// The activation step, which goes first.
        step: Vec<(u32, AnyRequest)>,
        /// The content type the host lacks, after the surrounding text.
        content_type: Vec<(u32, AnyRequest)>,
    }

    impl Field {
        /// A disabled text input whose host has the text-input extension at
        /// version `extension`, if any.
        async fn new(extension: Option<u32>) -> Self {
            Self::offered(Host {
                text_input_extension: extension,
                keyboard_extension_with_keymap: false,
            })
            .await
        }

        /// A disabled text input with what `host` offers.
        async fn offered(host: Host) -> Self {
            let mut s = Session::new();
            // Offset the guest's IDs from the host's, so that a request
            // that names a guest object where its host object belongs
            // fails.
            for _ in 0..100 {
                s.new_id();
            }
            let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
            let keyboard = s.new_id();
            let keyboard = Object {
                id: keyboard,
                host: s
                    .create(wl_seat::req::get_keyboard(seat.id, keyboard))
                    .await,
            };
            let surface = s.surface().await;
            let manager = s
                .bind(
                    "zwp_text_input_manager_v1",
                    zwp_text_input_manager_v1::VERSION,
                )
                .await;
            if let Some(version) = host.text_input_extension {
                s.advertise("zcr_text_input_extension_v1", version).await;
                // The proxy binds it for itself.
                s.host_received();
            }
            let mut f = Self {
                s,
                ti: 0,
                host: 0,
                host_ext: 0,
                seat,
                keyboard,
                surface,
                manager,
                extended_keyboard: 0,
            };
            let keyboard_extension = match host.keyboard_extension_with_keymap {
                true => Some(f.offer_keyboard_extension_with_keymap().await),
                false => None,
            };
            assert_eq!(f.enter_surface().await, []);
            if let Some(extension) = keyboard_extension {
                // The keyboard's first enter gets it its extended keyboard.
                let host = f.s.host_received();
                let [(
                    sender,
                    AnyRequest::ZcrKeyboardExtensionV1(KeyboardExtension::GetExtendedKeyboard {
                        id,
                        keyboard,
                    }),
                )] = host[..]
                else {
                    panic!("the host got no extended keyboard: {host:?}");
                };
                assert_eq!((sender, keyboard), (extension, f.keyboard.host));
                f.extended_keyboard = id;
            }
            let ti = f.s.new_id();
            f.create_text_input(ti).await;
            // The offset still keeps every guest ID above every host ID.
            let host = [
                f.seat.host,
                f.keyboard.host,
                f.surface.host,
                f.host,
                f.host_ext,
                f.extended_keyboard,
            ];
            let guest = [f.seat.id, f.keyboard.id, f.surface.id, f.manager, f.ti];
            assert!(
                host.iter().max() < guest.iter().min(),
                "{host:?}, {guest:?}"
            );
            f
        }

        /// A disabled text input whose host has the newest extension.
        async fn disabled() -> Self {
            Self::new(EXTENSION).await
        }

        /// A text input the guest enabled, which the host activated.
        async fn enabled() -> Self {
            let mut f = Self::disabled().await;
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(f.s.host_received(), f.enable_requests(1));
            f.answered(1);
            f
        }

        /// A text input the host activated, whose guest then committed the
        /// surrounding text `text` with `cursor` and `anchor`, so that each
        /// `done` is numbered 2.
        async fn with_surrounding_text(text: &str, cursor: i32, anchor: i32) -> Self {
            let mut f = Self::enabled().await;
            f.s.guest([
                v3::set_surrounding_text(f.ti, text, cursor, anchor).unwrap(),
                v3::commit(f.ti),
            ])
            .await;
            let mut expected = vec![f.surrounding_text(text, cursor as u32, anchor as u32)];
            expected.extend(f.content_type((0, 0), true));
            expected.push(f.commit_state(2));
            assert_eq!(f.s.host_received(), expected);
            f.answered(2);
            f
        }

        /// A text input the guest enabled with the surrounding text "가",
        /// which the host activated, on a keyboard whose keys the host
        /// reports to the proxy before its input method takes them
        /// ([`Host::keyboard_extension_with_keymap`]).
        async fn typing() -> Self {
            let mut f = Self::offered(Host {
                text_input_extension: EXTENSION,
                keyboard_extension_with_keymap: true,
            })
            .await;
            f.start_typing().await;
            f
        }

        /// The guest enables the field with the surrounding text "가",
        /// and the host activates it, as for [`Self::typing`].
        async fn start_typing(&mut self) {
            let f = self;
            f.s.guest([
                v3::enable(f.ti),
                v3::set_surrounding_text(f.ti, "가", 3, 3).unwrap(),
                v3::commit(f.ti),
            ])
            .await;
            // The default content type, spelled out: the traces' other
            // expectations take it from `map_v3_content_type`.
            let input_type = Ext::SetInputType {
                input_type: zcr_extended_text_input_v1::INPUT_TYPE_TEXT,
                input_mode: zcr_extended_text_input_v1::INPUT_MODE_DEFAULT,
                input_flags: zcr_extended_text_input_v1::INPUT_FLAGS_NONE,
                learning_mode: zcr_extended_text_input_v1::LEARNING_MODE_ENABLED,
                inline_composition_support:
                    zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
            };
            let expected = [
                f.activate(),
                f.surrounding_text("가", 3, 3),
                f.ext(Ext::SetSurroundingTextSupport { support: 1 }),
                f.ext(input_type),
                f.no_cursor_rectangle(),
                f.commit_state(1),
            ];
            assert_eq!(f.s.host_received(), expected);
            f.answered(1);
        }

        /// The host advertises the keyboard extension, which the proxy
        /// binds, and sends the keyboard the default keymap, which the
        /// guest gets; returns the extension's host object.
        async fn offer_keyboard_extension_with_keymap(&mut self) -> u32 {
            self.s
                .advertise(
                    "zcr_keyboard_extension_v1",
                    zcr_keyboard_extension_v1::VERSION,
                )
                .await;
            let host = self.s.host_received();
            let [(
                _,
                AnyRequest::WlRegistry(wl_registry::Request::Bind {
                    id: (_, _, extension),
                    ..
                }),
            )] = host[..]
            else {
                panic!("the proxy did not bind the keyboard extension: {host:?}");
            };

            let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
            let flags = xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS;
            let keymap =
                xkbcommon::xkb::Keymap::new_from_names(&context, "", "", "", "", None, flags)
                    .expect("the default keymap");
            let mut keymap = keymap
                .get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1)
                .into_bytes();
            // The size counts the terminating NUL.
            keymap.push(0);
            let size = keymap.len() as u32;
            let file = crate::test_support::shm_pool_file(&keymap);
            let fd = OwnedFd::from(file.try_clone().expect("duplicate the keymap file"));
            let format = wl_keyboard::KEYMAP_FORMAT_XKB_V1;
            let event = wl_keyboard::evt::keymap(self.keyboard.host, format, fd, size);
            self.s.host([event]).await;
            let fd = OwnedFd::from(file);
            let keymap = wl_keyboard::Event::Keymap { format, fd, size };
            assert_eq!(self.s.guest_received(), [(self.keyboard.id, keymap.into())]);
            extension
        }

        /// The guest creates text input `ti` for the seat, which the
        /// keyboard focus enters at once.
        async fn create_text_input(&mut self, ti: u32) {
            self.ti = ti;
            self.s
                .guest([zwp_text_input_manager_v3::req::get_text_input(
                    self.manager,
                    self.ti,
                    self.seat.id,
                )])
                .await;
            let host = self.s.host_received();
            let (_, AnyRequest::ZwpTextInputManagerV1(V1Manager::CreateTextInput { id })) = host[0]
            else {
                panic!("the host got no text input: {host:?}");
            };
            self.host = id;
            self.host_ext = match host[1..] {
                [] => 0,
                [(
                    _,
                    AnyRequest::ZcrTextInputExtensionV1(ExtManager::GetExtendedTextInput {
                        id,
                        text_input,
                    }),
                )] if text_input == self.host => id,
                _ => panic!("the host got no extended text input: {host:?}"),
            };
            assert_eq!(
                self.s.guest_received(),
                self.events([V3::Enter {
                    surface: self.surface.id
                }])
            );
        }

        /// The text input the field drives.
        fn text_input(&self) -> TextInput {
            let Self {
                ti, host, host_ext, ..
            } = *self;
            TextInput { ti, host, host_ext }
        }

        /// Drive `text_input` from now on.
        fn select(&mut self, TextInput { ti, host, host_ext }: TextInput) {
            (self.ti, self.host, self.host_ext) = (ti, host, host_ext);
        }

        /// `events` of the guest text input.
        fn events(&self, events: impl IntoIterator<Item = V3>) -> Vec<(u32, AnyEvent)> {
            events
                .into_iter()
                .map(|event| (self.ti, event.into()))
                .collect()
        }

        /// The guest gets the `done` that answers its commit `serial`, and
        /// nothing else.
        fn answered(&mut self, serial: u32) {
            let events = self.events([V3::Done { serial }]);
            assert_eq!(self.s.guest_received(), events);
        }

        /// `request` of the host text input.
        fn v1(&self, request: V1) -> (u32, AnyRequest) {
            (self.host, request.into())
        }

        /// `request` of the host extension.
        fn ext(&self, request: Ext) -> (u32, AnyRequest) {
            (self.host_ext, request.into())
        }

        fn activate(&self) -> (u32, AnyRequest) {
            self.v1(V1::Activate {
                seat: self.seat.host,
                surface: self.surface.host,
            })
        }

        fn deactivate(&self) -> (u32, AnyRequest) {
            self.v1(V1::Deactivate {
                seat: self.seat.host,
            })
        }

        fn surrounding_text(&self, text: &str, cursor: u32, anchor: u32) -> (u32, AnyRequest) {
            self.v1(V1::SetSurroundingText {
                text: text.into(),
                cursor,
                anchor,
            })
        }

        /// The unset cursor rectangle that enable and disable leave.
        fn no_cursor_rectangle(&self) -> (u32, AnyRequest) {
            self.v1(V1::SetCursorRectangle {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            })
        }

        fn commit_state(&self, serial: u32) -> (u32, AnyRequest) {
            self.v1(V1::CommitState { serial })
        }

        /// The extension requests that set content type `(hint, purpose)`
        /// with surrounding-text `support`.
        fn content_type(
            &self,
            (hint, purpose): (u32, u32),
            support: bool,
        ) -> [(u32, AnyRequest); 2] {
            let (_, _, input_type, input_mode, input_flags, learning_mode) =
                map_v3_content_type(hint, purpose);
            [
                self.ext(Ext::SetSurroundingTextSupport {
                    support: u32::from(support),
                }),
                self.ext(Ext::SetInputType {
                    input_type,
                    input_mode,
                    input_flags,
                    learning_mode,
                    inline_composition_support:
                        zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
                }),
            ]
        }

        /// What the host gets for the guest's commit `serial` that enables
        /// or disables the field: the activation step, the cleared
        /// surrounding text, the content type the host lacks, the cleared
        /// cursor rectangle and the commit.
        fn reset(
            &self,
            serial: u32,
            Reset { step, content_type }: Reset,
        ) -> Vec<(u32, AnyRequest)> {
            let mut requests = step;
            requests.push(self.surrounding_text("", 0, 0));
            requests.extend(content_type);
            requests.extend([self.no_cursor_rectangle(), self.commit_state(serial)]);
            requests
        }

        /// What the host gets when the guest's commit `serial` enables a
        /// field with no content type and the host activates it.
        fn enable_requests(&self, serial: u32) -> Vec<(u32, AnyRequest)> {
            let content_type = self.content_type((0, 0), false);
            let step = vec![self.activate()];
            let content_type = content_type.into();
            self.reset(serial, Reset { step, content_type })
        }

        /// What the host gets when the guest's commit `serial` disables an
        /// active field, which the barrier `callback` drains.
        fn disable_requests(&self, callback: u32, serial: u32) -> Vec<(u32, AnyRequest)> {
            let step = vec![self.deactivate(), sync(callback)];
            self.reset(
                serial,
                Reset {
                    step,
                    ..Reset::default()
                },
            )
        }

        fn password_content_type(&self) -> [(u32, AnyRequest); 2] {
            self.content_type((0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD), false)
        }

        /// What the host gets when the proxy activates the password field
        /// again after the guest's commit `serial`.
        fn password_after_activate(&self, serial: u32) -> Vec<(u32, AnyRequest)> {
            let mut requests = vec![self.activate()];
            requests.extend(self.password_content_type());
            requests.push(self.commit_state(serial));
            requests
        }

        /// The guest releases its seat and the host deletes it.
        async fn release_seat(&mut self) {
            self.s.guest([wl_seat::req::release(self.seat.id)]).await;
            assert_eq!(
                self.s.host_received(),
                [(self.seat.host, wl_seat::Request::Release {}.into())]
            );
            self.s
                .host([wl_display::evt::delete_id(WL_DISPLAY_ID, self.seat.host)])
                .await;
            assert_eq!(
                self.s.guest_received(),
                [(
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: self.seat.id }.into()
                )]
            );
        }

        /// The guest binds a new `wl_seat` under the ID of its ended seat;
        /// returns what the host received besides the bind.
        async fn bind_seat_again(&mut self) -> Vec<(u32, AnyRequest)> {
            let registry = self.s.registry().await;
            let name = self.s.advertise("wl_seat", wl_seat::VERSION).await;
            let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
                self.s.guest_received()[..]
            else {
                panic!("the guest was not advertised the seat");
            };
            let bind = wl_registry::req::bind(registry.id, name, "wl_seat", version, self.seat.id);
            self.s.guest([bind.unwrap()]).await;
            let mut host = self.s.host_received();
            let (
                _,
                AnyRequest::WlRegistry(wl_registry::Request::Bind {
                    id: (_, _, seat), ..
                }),
            ) = host.remove(0)
            else {
                panic!("the host got no bind: {host:?}");
            };
            self.seat.host = seat;
            host
        }

        /// The host's keyboard focus leaves the surface.
        async fn keyboard_leave(&mut self) {
            let leave = wl_keyboard::evt::leave(self.keyboard.host, 2, self.surface.host);
            self.s.host([leave]).await;
            let surface = self.surface.id;
            assert_eq!(
                self.s.guest_received(),
                [
                    (
                        self.keyboard.id,
                        wl_keyboard::Event::Leave { serial: 2, surface }.into()
                    ),
                    (self.ti, V3::Leave { surface }.into()),
                ]
            );
        }

        /// The host gives the keyboard's focus to the surface; returns what
        /// the guest got after the keyboard's `enter` and the modifiers the
        /// proxy sends with it.
        async fn enter_surface(&mut self) -> Vec<(u32, AnyEvent)> {
            let enter = wl_keyboard::evt::enter(self.keyboard.host, 1, self.surface.host, &[]);
            self.s.host([enter.unwrap()]).await;
            let mut events = self.s.guest_received();
            let keyboard_events = events.drain(..2.min(events.len())).collect::<Vec<_>>();
            let enter = wl_keyboard::Event::Enter {
                serial: 1,
                surface: self.surface.id,
                keys: Vec::new(),
            };
            let modifiers = wl_keyboard::Event::Modifiers {
                serial: 1,
                mods_depressed: 0,
                mods_latched: 0,
                mods_locked: 0,
                group: 0,
            };
            assert_eq!(
                keyboard_events,
                [
                    (self.keyboard.id, enter.into()),
                    (self.keyboard.id, modifiers.into())
                ]
            );
            events
        }

        /// The host's keyboard focus enters a new surface, which the field
        /// follows from now on.
        async fn keyboard_enter_new_surface(&mut self) {
            self.surface = self.s.surface().await;
            self.keyboard_enter().await;
        }

        /// The host's keyboard focus enters the surface again.
        async fn keyboard_enter(&mut self) {
            let surface = self.surface.id;
            assert_eq!(
                self.enter_surface().await,
                self.events([V3::Enter { surface }])
            );
        }

        /// The guest gets a keyboard of the seat, to which the host gives
        /// the focus on the surface.
        async fn focus_keyboard_of_seat(&mut self) {
            let keyboard = self.s.new_id();
            let host = self
                .s
                .create(wl_seat::req::get_keyboard(self.seat.id, keyboard))
                .await;
            self.keyboard = Object { id: keyboard, host };
            assert_eq!(self.enter_surface().await, []);
        }

        /// The host's `button` event of `pointer`, of the left button in
        /// `state`.
        fn button(pointer: Object, state: u32) -> Outgoing {
            wl_pointer::evt::button(pointer.host, 4, 500, BTN_LEFT, state)
        }

        /// The `button` event `pointer` gets for [`Self::button`].
        fn guest_button(pointer: Object, state: u32) -> (u32, AnyEvent) {
            let button = wl_pointer::Event::Button {
                serial: 4,
                time: 500,
                button: BTN_LEFT,
                state,
            };
            (pointer.id, button.into())
        }

        /// A host `wl_touch.down` of `touch` on `surface`, of touch point
        /// `id`.
        fn down(touch: Object, surface: Object, id: i32) -> Outgoing {
            wl_touch::evt::down(touch.host, 6, 600, surface.host, id, 256, 512)
        }

        /// The `down` event `touch` gets for [`Self::down`].
        fn guest_down(touch: Object, surface: Object, id: i32) -> (u32, AnyEvent) {
            let down = wl_touch::Event::Down {
                serial: 6,
                time: 600,
                surface: surface.id,
                id,
                x: 256,
                y: 512,
            };
            (touch.id, down.into())
        }

        /// The host completes the drain barrier `callback`.
        async fn drain(&mut self, callback: u32) {
            self.s.host([wl_callback::evt::done(callback, 0)]).await;
        }

        /// The host answers the syncs the proxy sent with the recoveries
        /// of held keys that waited, the only requests it got: the `done`
        /// decides the one that still waits, if any, as an autorepeat's
        /// ([`crate::handler::ime_keys::PendingRecovery`]).
        async fn answer_recovery_syncs(&mut self) {
            for (sender, request) in self.s.host_received() {
                let AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) = request else {
                    panic!("the host got {request:?} from {sender}, not a recovery's sync");
                };
                self.s.host([wl_callback::evt::done(callback, 0)]).await;
            }
        }

        /// The host input method's `preedit_string`.
        fn preedit(&self, text: &str, commit: &str) -> Outgoing {
            v1::preedit_string(self.host, 0, text, commit).unwrap()
        }

        /// The host input method's `commit_string`.
        fn commit(&self, text: &str) -> Outgoing {
            v1::commit_string(self.host, 0, text).unwrap()
        }

        /// The host input method's `delete_surrounding_text` of the bytes
        /// `index..index + length`, as Exo sends it.
        fn delete_range(&self, index: i32, length: u32) -> Outgoing {
            v1::delete_surrounding_text(self.host, index, length)
        }

        /// The host input method's `confirm_preedit`.
        fn confirm(&self) -> Outgoing {
            ext::confirm_preedit(self.host_ext, 1)
        }

        /// The extended keyboard's `peek_key` of `k`, which the host
        /// sends before its input method can take the key.
        fn peek(&self, k: Stroke) -> Outgoing {
            let peek_key = zcr_extended_keyboard_v1::evt::peek_key;
            peek_key(self.extended_keyboard, k.serial, k.time, k.key, k.state)
        }

        /// The host keyboard's `key` of `k`.
        fn key(&self, k: Stroke) -> Outgoing {
            wl_keyboard::evt::key(self.keyboard.host, k.serial, k.time, k.key, k.state)
        }

        /// The host input method's `keysym` `sym` of `k`, whose serial
        /// and time Exo sends in each other's arguments.
        fn keysym(&self, sym: u32, k: Stroke) -> Outgoing {
            v1::keysym(self.host, k.time, k.serial, sym, k.state, 0)
        }

        /// The `key` of `k` the guest keyboard gets.
        fn guest_key(&self, k: Stroke) -> (u32, AnyEvent) {
            key(self.keyboard.id, k.key, k.state, k.serial, k.time)
        }

        /// What the guest gets when an empty confirmation stands for the
        /// held key `held`, the key's latest peek: a press and a release,
        /// both with the serial and time of that peek, and `done`.
        fn recovered(&self, held: Stroke) -> Vec<(u32, AnyEvent)> {
            let press = Stroke {
                state: PRESSED,
                ..held
            };
            let release = Stroke {
                state: RELEASED,
                ..held
            };
            let mut events = vec![self.guest_key(press), self.guest_key(release)];
            events.extend(self.events([DONE]));
            events
        }

        /// The proxy's `ack_key` of key `serial`, which the guest
        /// `handled`.
        fn ack(&self, serial: u32, handled: bool) -> (u32, AnyRequest) {
            let handled = match handled {
                true => zcr_extended_keyboard_v1::HANDLED_STATE_HANDLED,
                false => zcr_extended_keyboard_v1::HANDLED_STATE_NOT_HANDLED,
            };
            let ack = zcr_extended_keyboard_v1::Request::AckKey { serial, handled };
            (self.extended_keyboard, ack.into())
        }

        /// The IME repeat lease of `key` on the host keyboard, which no
        /// peer sees until the key's next confirmation.
        fn repeat_owner(&mut self, key: u32) -> Option<u32> {
            let keyboard = HostId(self.keyboard.host);
            self.s.ctx().key_generations.ime_repeat_owner(keyboard, key)
        }

        /// The owner the proxy assigned the guest's press of `key` on the
        /// host keyboard, which no peer sees.
        fn owner(&mut self, key: u32) -> Option<GuestKeyOwner> {
            let keyboard = HostId(self.keyboard.host);
            self.s.ctx().guest_key_owner(keyboard, key)
        }

        /// The proxy's state of the text input, for state no peer sees.
        fn state(&mut self) -> &crate::ime::TextInput {
            let ti = TiId(self.ti);
            self.s.ctx().ime.get(ti).expect("the text input")
        }

        /// The host gives the held Backspace to its input method, which
        /// empties a preedit: the proxy leases Backspace's repeat to the
        /// text input. Returns the press.
        async fn lease_backspace_repeat(&mut self) -> Stroke {
            let press = Stroke {
                key: BACKSPACE,
                state: PRESSED,
                serial: 700,
                time: 1_234,
            };
            let events = [
                self.peek(press),
                self.preedit("가", ""),
                self.preedit("", ""),
            ];
            self.s.host(events).await;
            let expected = [preedit("가"), DONE, no_preedit(), DONE];
            assert_eq!(self.s.guest_received(), self.events(expected));
            assert_eq!(self.repeat_owner(BACKSPACE), Some(self.ti));
            press
        }

        /// Whether the proxy holds `key` of the host keyboard, which no peer
        /// sees until the key's next event.
        fn held(&mut self, key: u32) -> bool {
            let keyboard = HostId(self.keyboard.host);
            self.s.ctx().key_generations.physically_held(keyboard, key)
        }
    }

    /// A key event of the host keyboard.
    #[derive(Clone, Copy, Debug)]
    struct Stroke {
        key: u32,
        state: u32,
        serial: u32,
        time: u32,
    }

    /// The evdev code of the space bar.
    const SPACE: u32 = 57;
    /// The evdev code of the left mouse button.
    const BTN_LEFT: u32 = 0x110;

    /// The `wl_display.sync` that creates drain barrier `callback`.
    fn sync(callback: u32) -> (u32, AnyRequest) {
        (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into())
    }

    /// The callback of the one drain barrier among `requests`.
    fn barrier(requests: &[(u32, AnyRequest)]) -> u32 {
        let callbacks = requests
            .iter()
            .filter_map(|request| match request {
                (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })) => {
                    Some(*callback)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [callback] = callbacks[..] else {
            panic!("no one drain barrier in {requests:?}");
        };
        callback
    }

    /// The guest's `preedit_string` of `text`, with the cursor at its end.
    fn preedit(text: &str) -> V3 {
        let end = text.len() as i32;
        preedit_at(text, end, end)
    }

    fn preedit_at(text: &str, cursor_begin: i32, cursor_end: i32) -> V3 {
        V3::PreeditString {
            text: Some(text.into()),
            cursor_begin,
            cursor_end,
        }
    }

    /// The guest's `preedit_string` that clears its preedit.
    fn no_preedit() -> V3 {
        preedit_at("", 0, 0)
    }

    fn commit(text: &str) -> V3 {
        V3::CommitString {
            text: Some(text.into()),
        }
    }
    #[tokio::test]
    async fn enable_is_not_activated_until_commit() {
        let mut f = Field::disabled().await;
        f.s.guest([v3::enable(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);

        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        f.answered(1);
    }

    /// A focus change deactivates the host behind a drain barrier: the
    /// host events sent before it reach no one, and the next activation,
    /// which follows the guest's new state, waits for it.
    #[tokio::test]
    async fn focus_generation_barrier_drops_old_events_before_reactivation() {
        let mut f = Field::enabled().await;
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);

        // Events the host sent before the barrier.
        f.s.host([f.preedit("old", ""), f.commit("old")]).await;
        assert_eq!(f.s.guest_received(), []);

        // The focus enters another surface, where the guest enables the
        // field while the barrier drains: its state reaches the host first,
        // without an activation.
        f.keyboard_enter_new_surface().await;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_surrounding_text(f.ti, "new", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [
                f.surrounding_text("new", 3, 3),
                f.no_cursor_rectangle(),
                f.commit_state(2),
            ]
        );
        f.answered(2);

        // Then the activation, and the content type it resets.
        f.drain(callback).await;
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);

        f.s.host([f.preedit("new", "")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([preedit("new"), V3::Done { serial: 2 }])
        );
    }

    /// Exo drops a content type sent while no input method is attached:
    /// the disable sends none, neither does an enable while the
    /// deactivation drains, and the activation the barrier releases sends
    /// it once.
    #[tokio::test]
    async fn a_deferred_activation_replays_the_committed_content_type() {
        use zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD as PASSWORD;
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));

        f.s.guest([
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), f.reset(3, Reset::default()));
        f.answered(3);

        f.drain(callback).await;
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, PASSWORD), false));
        expected.push(f.commit_state(3));
        assert_eq!(f.s.host_received(), expected);
        let (_, _, input_type, _, _, learning_mode) = map_v3_content_type(0, PASSWORD);
        assert_eq!(input_type, zcr_extended_text_input_v1::INPUT_TYPE_PASSWORD);
        assert_eq!(
            learning_mode,
            zcr_extended_text_input_v1::LEARNING_MODE_DISABLED
        );
    }

    /// A text input the guest enabled on a password field, which the host
    /// activated and attached its input method to.
    async fn attached_password_field() -> Field {
        let mut f = Field::disabled().await;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        let content_type = f.password_content_type().into();
        assert_eq!(
            f.s.host_received(),
            f.reset(
                1,
                Reset {
                    step: vec![f.activate()],
                    content_type
                }
            )
        );
        f.answered(1);
        f.s.host([v1::enter(f.host, f.surface.host)]).await;
        f
    }

    #[tokio::test]
    async fn the_enter_answering_an_activate_resends_nothing() {
        let mut f = attached_password_field().await;
        assert_eq!(f.s.host_received(), []);

        // The content type sent after the activate is the one Exo applied.
        f.s.guest([
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);
        f.answered(2);
    }

    /// Exo detached on its own, as when its input method is destroyed, and
    /// has no surface left to attach to: the proxy activates again and
    /// resends the content type the new attachment reset.
    #[tokio::test]
    async fn an_exo_detach_activates_again_with_the_content_type() {
        let mut f = attached_password_field().await;
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The enter answering that activate resends nothing.
        f.s.host([v1::enter(f.host, f.surface.host)]).await;
        assert_eq!(f.s.host_received(), []);
        // The new generation takes host events.
        f.s.host([f.commit("가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("가"), V3::Done { serial: 1 }])
        );
    }

    /// Exo detaches while the guest shows a preedit that its detach did
    /// not confirm, as below v11 of the extension once a report it could
    /// not match dropped its composition (E4, E6): the guest commits it.
    #[tokio::test]
    async fn an_exo_detach_commits_the_guest_preedit() {
        let mut f = attached_password_field().await;

        // Without a preedit the guest is told nothing.
        f.s.host([v1::leave(f.host), v1::enter(f.host, f.surface.host)])
            .await;
        assert_eq!(f.s.guest_received(), []);
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The input method's process restarts mid-composition.
        f.s.host([f.preedit("가", "가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([preedit("가"), V3::Done { serial: 1 }])
        );
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }])
        );
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The composition went with it: a commit clears no preedit.
        f.s.host([f.commit("나")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("나"), V3::Done { serial: 1 }])
        );
    }

    /// The focus leaves a field while the guest shows a preedit: the guest
    /// commits it before the keyboard's `leave` and the text input's (F1),
    /// while the field still has the focus. Exo's detach confirms the
    /// composition too (E6), but the keyboard hears of the focus change
    /// first (E12), so its confirmation arrives while the link drains, and
    /// is dropped.
    #[tokio::test]
    async fn a_focus_loss_commits_the_shown_preedit_before_leave() {
        let mut f = Field::enabled().await;
        f.s.host([f.preedit("가", "가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([preedit("가"), V3::Done { serial: 1 }])
        );
        let leave = wl_keyboard::evt::leave(f.keyboard.host, 2, f.surface.host);
        f.s.host([leave]).await;
        let surface = f.surface.id;
        let leave = wl_keyboard::Event::Leave { serial: 2, surface };
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.push((f.keyboard.id, leave.into()));
        expected.extend(f.events([V3::Leave { surface }]));
        assert_eq!(f.s.guest_received(), expected);
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);

        f.s.host([f.confirm(), v1::leave(f.host)]).await;
        assert_eq!(f.s.guest_received(), []);
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
    }

    /// Exo's text input hears of a focus change before the keyboard: its
    /// `leave` arrives first, and the proxy activates again on the surface
    /// Exo has just left, which Exo keeps without attaching.
    #[tokio::test]
    async fn an_exo_detach_before_the_keyboard_leave_activates_then_drains() {
        let mut f = attached_password_field().await;
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The keyboard leave then deactivates it behind a barrier.
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);

        // Exo never attached, so it sends no leave, and the drain ends with
        // nothing more to send.
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
        // The host is inactive: it neither forwards nor takes its events.
        f.s.host([f.commit("가")]).await;
        assert_eq!(f.s.guest_received(), []);
    }

    /// Exo detached while the guest's seat cannot carry an `activate`:
    /// nothing is sent, not even a changed content type, until it can.
    #[tokio::test]
    async fn a_content_type_waits_for_exo_to_attach_again() {
        let mut f = attached_password_field().await;
        f.release_seat().await;
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), []);

        f.s.guest([
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_NORMAL),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);
        f.answered(2);
    }

    #[tokio::test]
    async fn the_leave_answering_a_deactivate_is_ignored() {
        let mut f = attached_password_field().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));

        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), []);
        // The barrier still drains the generation: an enable waits for it.
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.reset(3, Reset::default()));
        f.answered(3);
        f.drain(callback).await;
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, 0), false));
        expected.push(f.commit_state(3));
        assert_eq!(f.s.host_received(), expected);
    }

    /// The text input's seat ends, and the guest binds a new `wl_seat`
    /// under its ID, which gives the text input a host seat again.
    #[tokio::test]
    async fn an_activation_deferred_for_an_unmapped_seat_sends_the_content_type() {
        let mut f = Field::disabled().await;
        f.release_seat().await;

        // The seat has no host object: nothing can activate, which the
        // proxy warns of once.
        f.s.guest([
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), f.reset(1, Reset::default()));
        f.answered(1);
        // Only the log shows the warning, so read the flag that makes it
        // once.
        assert!(f.state().warned_for_test().0);

        // Once it has one, the text input activates, and its content type
        // follows the activation.
        let host = f.bind_seat_again().await;
        let mut expected = vec![f.activate()];
        expected.extend(f.password_content_type());
        expected.push(f.commit_state(1));
        assert_eq!(host, expected);
    }

    /// One deactivation owns one barrier however the guest's state changes
    /// while it drains, and a field disabled by then is not activated.
    #[tokio::test]
    async fn activation_barrier_coalesces_changes_and_honors_final_disabled_target() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));

        // The guest enables the field on two other surfaces in turn, then
        // disables it.
        for serial in [3, 4] {
            f.keyboard_leave().await;
            f.keyboard_enter_new_surface().await;
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(f.s.host_received(), f.reset(serial, Reset::default()));
            f.answered(serial);
        }
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.reset(5, Reset::default()));

        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
    }

    /// Exo has one seat and gives its input method to the text input that
    /// activated last, so a second text input, of the same or another
    /// guest seat, cannot take it from an enabled one.
    #[tokio::test]
    async fn second_text_input_enable_on_the_connection_is_ignored() {
        for other_seat in [false, true] {
            let mut f = Field::enabled().await;
            let first = f.text_input();
            if other_seat {
                f.seat = f.s.bind_paired("wl_seat", wl_seat::VERSION).await;
                f.focus_keyboard_of_seat().await;
            }
            let id = f.s.new_id();
            f.create_text_input(id).await;

            // A rejected enable still counts: its commit is the first.
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(
                f.s.host_received(),
                f.reset(1, Reset::default()),
                "another seat: {other_seat}"
            );

            // Its disable takes nothing from the first, which Exo still
            // gives its input method.
            f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(f.s.host_received(), f.reset(2, Reset::default()));
            f.select(first);
            f.s.host([f.commit("가")]).await;
            assert_eq!(
                f.s.guest_received(),
                f.events([commit("가"), V3::Done { serial: 1 }])
            );
        }
    }

    /// The text inputs of a seat follow its focus in ascending ID order,
    /// whatever order the guest created them in, so what the proxy sends
    /// never depends on how it hashes them. With eight, an order that
    /// depends on hashing is all but certain to differ; two would match it
    /// half the time.
    #[tokio::test]
    async fn the_text_inputs_of_a_seat_follow_its_focus_in_id_order() {
        let mut f = Field::enabled().await;
        let first = f.text_input();
        let mut ids = vec![first.ti];
        ids.extend((1..8).map(|_| f.s.new_id()));
        for &id in ids[1..].iter().rev() {
            f.create_text_input(id).await;
        }
        f.select(first);
        let (keyboard, surface) = (f.keyboard, f.surface.id);

        let leave = wl_keyboard::evt::leave(keyboard.host, 2, f.surface.host);
        f.s.host([leave]).await;
        let mut expected: Vec<(u32, AnyEvent)> = vec![(
            keyboard.id,
            wl_keyboard::Event::Leave { serial: 2, surface }.into(),
        )];
        expected.extend(ids.iter().map(|&ti| (ti, V3::Leave { surface }.into())));
        assert_eq!(f.s.guest_received(), expected);
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);

        let entered: Vec<(u32, AnyEvent)> = ids
            .iter()
            .map(|&ti| (ti, V3::Enter { surface }.into()))
            .collect();
        assert_eq!(f.enter_surface().await, entered);
        assert_eq!(f.s.host_received(), []);
    }

    /// A disable invalidates the surrounding text, at the host too, and
    /// deactivates it, but only at its commit.
    #[tokio::test]
    async fn disable_commit_invalidates_surrounding_text_and_deactivates() {
        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_surrounding_text(f.ti, "가", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), {
            let mut expected = vec![f.surrounding_text("가", 3, 3)];
            expected.extend(f.content_type((0, 0), true));
            expected.push(f.commit_state(2));
            expected
        });
        f.answered(2);

        f.s.guest([v3::disable(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);
        f.s.guest([v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        assert_eq!(host, f.disable_requests(barrier(&host), 3));
    }

    /// GTK3 enables an enabled field again on every click in it
    /// (`imwayland.c`, `released_cb`). That keeps the field, as mutter and
    /// KWin keep it: the host's composition runs on, with its content type,
    /// which before the v8 extension a resend would reset it with, so the
    /// `done` that answers the commit repeats the preedit, and the
    /// composition goes on from it. The guest never hides a syllable the
    /// input method still builds, and the host's reset commit still
    /// replaces it.
    #[tokio::test]
    async fn reenabling_an_enabled_field_keeps_its_composition() {
        let mut f = Field::enabled().await;
        f.s.host([
            v1::preedit_cursor(f.host, 0),
            f.preedit("가", "가"),
            v1::cursor_position(f.host, 1, 1),
        ])
        .await;
        let composing = || preedit_at("가", 0, 0);
        let expected = [composing(), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // The host keeps the field's content type, so none is sent.
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.reset(2, Reset::default()));
        let expected = [composing(), V3::Done { serial: 2 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        let done = || V3::Done { serial: 2 };
        f.s.host([f.preedit("간", "간")]).await;
        assert_eq!(f.s.guest_received(), f.events([preedit("간"), done()]));
        f.s.host([f.preedit("", "간")]).await;
        let expected = [no_preedit(), commit("간"), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // The preedit styling the host sent before the enable survives it.
        let selection = zwp_text_input_v1::PREEDIT_STYLE_SELECTION;
        f.s.host([v1::preedit_styling(f.host, 0, 3, selection)])
            .await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.reset(3, Reset::default()));
        let expected = [V3::Done { serial: 3 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([f.preedit("다", "다")]).await;
        let expected = [preedit_at("다", 0, 3), V3::Done { serial: 3 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// Each commit forwards the surrounding text it states; offsets that
    /// are not character boundaries within the text state none.
    #[tokio::test]
    async fn invalid_utf8_surrounding_offsets_do_not_replace_pending_state() {
        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_surrounding_text(f.ti, "가나", 3, 6).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let mut expected = vec![f.surrounding_text("가나", 3, 6)];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);
        f.answered(2);

        for (serial, (cursor, anchor)) in (3..).zip([(1, 1), (3, 7), (-1, 0)]) {
            f.s.guest([
                v3::set_surrounding_text(f.ti, "가", cursor, anchor).unwrap(),
                v3::commit(f.ti),
            ])
            .await;
            assert_eq!(
                f.s.host_received(),
                [f.commit_state(serial)],
                "{cursor}, {anchor}"
            );
            f.answered(serial);
        }
    }

    /// The cursor and anchor are byte offsets into the surrounding text,
    /// which replacing an invalid sequence would move, so surrounding text
    /// that is not UTF-8 cannot reach the host. It empties the field's
    /// surrounding text instead, rather than leave the host's input method
    /// editing text the guest no longer has, and the field keeps its
    /// support for surrounding text, so the host's content type stays. The
    /// session goes on, and the next valid text reaches the host.
    #[tokio::test]
    async fn surrounding_text_that_is_not_utf8_clears_the_hosts() {
        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_surrounding_text(f.ti, "가", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let mut expected = vec![f.surrounding_text("가", 3, 3)];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);
        f.answered(2);

        let latin1 = v3::set_surrounding_text(f.ti, "Cafe", 3, 3).unwrap();
        f.s.guest([
            v3::set_surrounding_text(f.ti, "나", 3, 3).unwrap(),
            respell(latin1, "Cafe", b"Caf\xe9"),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [f.surrounding_text("", 0, 0), f.commit_state(3)]
        );
        f.answered(3);

        f.s.guest([
            v3::set_surrounding_text(f.ti, "Café", 5, 5).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [f.surrounding_text("Café", 5, 5), f.commit_state(4)]
        );
        f.answered(4);
    }

    /// The v3 content type maps to the host's, which is told with it that
    /// a v3 client supports inline composition, and whether the field has
    /// surrounding text.
    #[tokio::test]
    async fn content_type_mapping_preserves_pin_privacy_and_inline_composition() {
        use zcr_extended_text_input_v1::{
            INPUT_FLAGS_HAS_BEEN_PASSWORD as SECRET, INPUT_FLAGS_SPELLCHECK_ON, INPUT_MODE_NUMERIC,
            INPUT_TYPE_NUMBER, INPUT_TYPE_PASSWORD, INPUT_TYPE_TEXT_AREA, LEARNING_MODE_DISABLED,
        };
        use zwp_text_input_v1::{CONTENT_HINT_SENSITIVE_DATA, CONTENT_PURPOSE_DIGITS};
        use zwp_text_input_v3::{
            CONTENT_HINT_MULTILINE, CONTENT_HINT_SPELLCHECK, CONTENT_PURPOSE_PASSWORD,
            CONTENT_PURPOSE_PIN,
        };
        // A text field, a terminal and a password field.
        use zcr_extended_text_input_v1::{
            INPUT_FLAGS_NONE, INPUT_MODE_DEFAULT, INPUT_TYPE_TEXT, LEARNING_MODE_ENABLED,
        };
        use zwp_text_input_v1 as v1_enum;
        use zwp_text_input_v3::CONTENT_PURPOSE_TERMINAL;
        let text = |v1_purpose| {
            let flags = INPUT_FLAGS_NONE;
            (
                0,
                v1_purpose,
                INPUT_TYPE_TEXT,
                INPUT_MODE_DEFAULT,
                flags,
                LEARNING_MODE_ENABLED,
            )
        };
        assert_eq!(
            map_v3_content_type(0, 0),
            text(v1_enum::CONTENT_PURPOSE_NORMAL)
        );
        assert_eq!(
            map_v3_content_type(0, CONTENT_PURPOSE_TERMINAL),
            text(v1_enum::CONTENT_PURPOSE_TERMINAL)
        );
        let (mode, purpose) = (INPUT_MODE_DEFAULT, v1_enum::CONTENT_PURPOSE_PASSWORD);
        assert_eq!(
            map_v3_content_type(0, CONTENT_PURPOSE_PASSWORD),
            (
                0,
                purpose,
                INPUT_TYPE_PASSWORD,
                mode,
                SECRET,
                LEARNING_MODE_DISABLED
            )
        );
        // v1 has no PIN purpose: digits, sensitive and never learned.
        assert_eq!(
            map_v3_content_type(0, CONTENT_PURPOSE_PIN),
            (
                CONTENT_HINT_SENSITIVE_DATA,
                CONTENT_PURPOSE_DIGITS,
                INPUT_TYPE_NUMBER,
                INPUT_MODE_NUMERIC,
                SECRET,
                LEARNING_MODE_DISABLED
            )
        );
        assert_eq!(
            map_v3_content_type(CONTENT_HINT_SENSITIVE_DATA, CONTENT_PURPOSE_PASSWORD),
            (
                0x80,
                8,
                INPUT_TYPE_PASSWORD,
                0,
                SECRET,
                LEARNING_MODE_DISABLED
            )
        );
        assert_eq!(
            map_v3_content_type(CONTENT_HINT_MULTILINE, 0).2,
            INPUT_TYPE_TEXT_AREA
        );
        assert_eq!(
            map_v3_content_type(CONTENT_HINT_SPELLCHECK, 0).4,
            INPUT_FLAGS_SPELLCHECK_ON,
            "v3 spellcheck must map to Chrome spellcheck_on"
        );

        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_content_type(f.ti, 0, CONTENT_PURPOSE_PIN),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [
                f.ext(Ext::SetSurroundingTextSupport {
                    support: zcr_extended_text_input_v1::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED
                }),
                f.ext(Ext::SetInputType {
                    input_type: INPUT_TYPE_NUMBER,
                    input_mode: INPUT_MODE_NUMERIC,
                    input_flags: SECRET,
                    learning_mode: LEARNING_MODE_DISABLED,
                    inline_composition_support:
                        zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
                }),
                f.commit_state(2),
            ]
        );
        f.answered(2);
    }

    /// GTK resends the unchanged terminal content type after IME events.
    /// Before v8, replaying `set_content_type` and
    /// `deprecated_set_input_type` resets Exo's active Korean preedit before
    /// the next key arrives, so a content type the host has, also one
    /// changed and changed back before the commit, is not resent.
    #[tokio::test]
    async fn repeated_content_type_does_not_reset_active_host_composition() {
        use zwp_text_input_v3::{CONTENT_PURPOSE_PIN as PIN, CONTENT_PURPOSE_TERMINAL as TERMINAL};
        for purposes in [&[TERMINAL][..], &[PIN, TERMINAL]] {
            let mut f = Field::enabled().await;
            f.s.guest([v3::set_content_type(f.ti, 0, TERMINAL), v3::commit(f.ti)])
                .await;
            let mut expected = Vec::from(f.content_type((0, TERMINAL), false));
            expected.push(f.commit_state(2));
            assert_eq!(f.s.host_received(), expected);
            f.answered(2);

            let mut requests = purposes
                .iter()
                .map(|&purpose| v3::set_content_type(f.ti, 0, purpose))
                .collect::<Vec<_>>();
            requests.push(v3::commit(f.ti));
            f.s.guest(requests).await;
            assert_eq!(f.s.host_received(), [f.commit_state(3)], "{purposes:?}");
            f.answered(3);
        }
    }

    #[tokio::test]
    async fn surrounding_text_support_follows_the_field_and_resends_only_on_change() {
        let mut f = Field::enabled().await;
        let edits = |f: &Field, serial, support| {
            let mut requests = Vec::from(f.content_type((0, 0), support));
            requests.push(f.commit_state(serial));
            requests
        };

        // Chromium enables with a content type and no surrounding text,
        // which the host is told unsupported ([`Field::enabled`]), and sends
        // the text in a later commit, with no new content type. Exo learns
        // the support only with a content type, so it is resent.
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abc", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let mut expected = vec![f.surrounding_text("abc", 3, 3)];
        expected.extend(edits(&f, 2, true));
        assert_eq!(f.s.host_received(), expected);
        f.answered(2);

        // While the support holds, the content type is not resent: before
        // v8 a resend changes Exo's type twice, which can reset its
        // composition.
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abcd", 4, 4).unwrap(),
            v3::set_content_type(f.ti, 0, 0),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [f.surrounding_text("abcd", 4, 4), f.commit_state(3)]
        );
        f.answered(3);

        // A disable and enable reset the editor state: the content type is
        // replayed with the support of the field's new, empty state.
        f.s.guest([
            v3::disable(f.ti),
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, 0),
            v3::commit(f.ti),
        ])
        .await;
        let content_type = f.content_type((0, 0), false).into();
        assert_eq!(
            f.s.host_received(),
            f.reset(
                4,
                Reset {
                    content_type,
                    ..Reset::default()
                }
            )
        );
        f.answered(4);
    }

    #[tokio::test]
    async fn a_host_without_surrounding_text_support_gets_no_resend() {
        // v8 has set_input_type but not set_surrounding_text_support.
        let mut f = Field::new(Some(8)).await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        let [_, set_input_type] = f.content_type((0, 0), false);
        let expected = f.reset(
            1,
            Reset {
                step: vec![f.activate()],
                content_type: vec![set_input_type],
            },
        );
        assert_eq!(f.s.host_received(), expected);
        f.answered(1);

        // The host was told no support, so gaining surrounding text changes
        // nothing it knows.
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abc", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [f.surrounding_text("abc", 3, 3), f.commit_state(2)]
        );
        f.answered(2);
    }

    /// The host requests of a content type for each extension version.
    #[tokio::test]
    async fn content_type_takes_one_host_request_per_extension_version() {
        use zwp_text_input_v3::CONTENT_PURPOSE_PIN as PIN;
        let (hint, purpose, input_type, input_mode, input_flags, learning_mode) =
            map_v3_content_type(0, PIN);
        for version in [1, 7, 8, 9] {
            let mut f = Field::new(Some(version)).await;
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            f.s.host_received();
            f.answered(1);
            f.s.guest([v3::set_content_type(f.ti, 0, PIN), v3::commit(f.ti)])
                .await;
            let set_content_type = f.v1(V1::SetContentType { hint, purpose });
            let deprecated_set_input_type = f.ext(Ext::DeprecatedSetInputType {
                input_type,
                input_mode,
                input_flags,
                learning_mode,
            });
            let [support, set_input_type] = f.content_type((0, PIN), false);
            let expected = match version {
                // v1 has no extension input-type request.
                1 => vec![set_content_type],
                // v2-v7 refine the v1 content type with
                // deprecated_set_input_type.
                7 => vec![set_content_type, deprecated_set_input_type],
                // v8's set_input_type is used in place of v1
                // set_content_type.
                8 => vec![set_input_type],
                // Exo applies surrounding-text support to the next
                // set_content_type or set_input_type and then resets it, so
                // nothing may come between.
                9 => vec![support, set_input_type],
                version => panic!("uncovered version {version}"),
            };
            let mut expected = expected;
            expected.push(f.commit_state(2));
            assert_eq!(f.s.host_received(), expected, "version {version}");
            f.answered(2);
        }
    }

    #[tokio::test]
    async fn destroy_drops_v3_request_and_cleans_host_extension_routing() {
        let mut f = Field::enabled().await;
        f.s.host([f.preedit("가", "")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.guest([v3::destroy(f.ti)]).await;
        let host = f.s.host_received();
        // The destroy is the final disable: the host extension goes after
        // the generation's barrier.
        let callback = barrier(&host);
        assert_eq!(
            host,
            [f.deactivate(), sync(callback), f.ext(Ext::Destroy {})]
        );
        assert_eq!(
            f.s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: f.ti }.into()
            )],
            "the guest-local destruction is acknowledged by wl_display"
        );

        // The host keeps the v1 object, which has no destructor, and the
        // extension's ID until its delete_id: a new text input takes
        // neither.
        let (old_host, old_host_ext) = (f.host, f.host_ext);
        let ti = f.s.new_id();
        f.create_text_input(ti).await;
        assert!(![old_host, old_host_ext].contains(&f.host));
        assert!(![old_host, old_host_ext].contains(&f.host_ext));
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);

        // A late confirm of the destroyed extension reaches no one, not even
        // the new text input's composition.
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        f.answered(1);
        f.s.host([f.preedit("나", ""), ext::confirm_preedit(old_host_ext, 0)])
            .await;
        let expected = [preedit("나"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // The host's delete_id frees the extension's ID, which the next
        // host object takes.
        f.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, old_host_ext)])
            .await;
        let ti = f.s.new_id();
        f.create_text_input(ti).await;
        assert_eq!(f.host, old_host_ext);
    }

    /// A destroyed text input whose seat ended, or awaits its
    /// `delete_id`, sends no `deactivate`: no seat can carry it.
    #[tokio::test]
    async fn destroy_after_seat_release_does_not_deactivate_destroyed_host_seat() {
        for deleted in [false, true] {
            let mut f = Field::enabled().await;
            if deleted {
                f.release_seat().await;
            } else {
                f.s.guest([wl_seat::req::release(f.seat.id)]).await;
                let release = (f.seat.host, wl_seat::Request::Release {}.into());
                assert_eq!(f.s.host_received(), [release]);
            }

            f.s.guest([v3::destroy(f.ti)]).await;
            assert_eq!(
                f.s.host_received(),
                [f.ext(Ext::Destroy {})],
                "deleted: {deleted}"
            );
            assert_eq!(
                f.s.guest_received(),
                [(
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: f.ti }.into()
                )]
            );
        }
    }

    /// The active text input's seat ended, and the guest reused its ID for
    /// a surface, which the host would reject as a seat.
    #[tokio::test]
    async fn a_seat_id_reused_for_another_object_is_not_sent_as_a_seat() {
        let mut f = Field::enabled().await;
        f.release_seat().await;
        let compositor = f.s.bind("wl_compositor", wl_compositor::VERSION).await;
        f.s.create(wl_compositor::req::create_surface(compositor, f.seat.id))
            .await;

        // Neither disabling the field nor enabling it again sends the ID:
        // each commit sends only its edits.
        for (serial, request) in [(2, v3::disable(f.ti)), (3, v3::enable(f.ti))] {
            f.s.guest([request, v3::commit(f.ti)]).await;
            assert_eq!(f.s.host_received(), f.reset(serial, Reset::default()));
        }
        f.answered(3);
        // The disable ended the host's generation: its events reach no one.
        f.s.host([f.commit("가")]).await;
        assert_eq!(f.s.guest_received(), []);
    }

    #[tokio::test]
    async fn text_input_creation_works_without_optional_chromeos_extension() {
        let mut f = Field::new(None).await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        let content_type = f.v1(V1::SetContentType {
            hint: 0,
            purpose: 0,
        });
        let expected = f.reset(
            1,
            Reset {
                step: vec![f.activate()],
                content_type: vec![content_type],
            },
        );
        assert_eq!(f.s.host_received(), expected);
        f.answered(1);
    }

    /// A leave is a hard boundary: what the guest requests until the next
    /// enter does not reach the host. Its commits still count
    /// (`v3_done_serial_counts_commits_made_while_unfocused`).
    #[tokio::test]
    async fn v3_requests_after_leave_are_ignored_until_next_enter() {
        let mut f = Field::disabled().await;
        f.keyboard_leave().await;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_surrounding_text(f.ti, "stale", 0, 0).unwrap(),
            v3::set_text_change_cause(f.ti, 1),
            v3::set_content_type(f.ti, 3, 4),
            v3::set_cursor_rectangle(f.ti, 1, 2, 3, 4),
            v3::disable(f.ti),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), []);
        // The next enter resets the pending state, so no peer can see it;
        // read it to pin that the leave bounds what the requests change.
        assert_eq!(
            f.state().pending_for_test(),
            &crate::ime::V3Pending::default(),
            "inactive v3 requests must not mutate double-buffered state"
        );
    }

    /// text-input-v3 numbers `done` by every commit on the object, so the
    /// commits a client makes while unfocused count as well.
    #[tokio::test]
    async fn v3_done_serial_counts_commits_made_while_unfocused() {
        let mut f = Field::enabled().await;

        // Focus leaves; the client disables and commits while unfocused.
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);

        // Focus enters again; the client enables and commits.
        f.keyboard_enter().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        f.answered(3);
        f.drain(callback).await;
        f.s.host_received();

        // The IME commits text; `done` carries the client's commit count.
        f.s.host([f.commit("가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("가"), V3::Done { serial: 3 }])
        );
    }

    /// A text-input-v3 client that, like Chromium's
    /// (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc`), sends its
    /// surrounding text and cursor rectangle only while the latest `done`
    /// names its latest commit (`DoneSerialEqualsCommitCount`), and on each
    /// `done` shows the preedit sent since the previous `done`, or none, as
    /// the protocol says.
    struct DoneCountingClient {
        /// Its `commit` requests.
        commits: u32,
        /// The serial of the latest `done`.
        done_serial: u32,
        /// The `preedit_string` since the latest `done`.
        pending_preedit: V3,
        /// The preedit it shows.
        preedit: V3,
    }

    impl DoneCountingClient {
        fn new() -> Self {
            Self {
                commits: 0,
                done_serial: 0,
                pending_preedit: no_preedit(),
                preedit: no_preedit(),
            }
        }

        /// Its next `commit` of text input `ti`.
        fn commit(&mut self, ti: u32) -> Outgoing {
            self.commits += 1;
            v3::commit(ti)
        }

        /// Apply `events` of its text input.
        fn receive(&mut self, events: Vec<(u32, AnyEvent)>) {
            for (_, event) in events {
                match event {
                    AnyEvent::ZwpTextInputV3(event @ V3::PreeditString { .. }) => {
                        self.pending_preedit = event;
                    }
                    AnyEvent::ZwpTextInputV3(V3::Done { serial }) => {
                        self.done_serial = serial;
                        self.preedit = std::mem::replace(&mut self.pending_preedit, no_preedit());
                    }
                    _ => {}
                }
            }
        }

        /// Whether it may send its state.
        fn may_send(&self) -> bool {
            self.done_serial == self.commits
        }
    }

    /// Every commit that leaves the field enabled gets a `done`, as mutter
    /// and KWin send, so a client that waits for one before it reports a
    /// caret move or an edit, as Chromium does, reports each at once
    /// rather than after the input method's next event. A commit that
    /// disables the field gets none, as from mutter.
    #[tokio::test]
    async fn every_commit_of_an_enabled_field_gets_a_done() {
        let mut f = Field::disabled().await;
        let mut client = DoneCountingClient::new();
        f.s.guest([v3::enable(f.ti), client.commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        let events = f.s.guest_received();
        assert_eq!(events, f.events([V3::Done { serial: 1 }]));
        client.receive(events);
        assert!(client.may_send());

        // The caret moves.
        let rect = v3::set_cursor_rectangle(f.ti, 10, 20, 1, 16);
        f.s.guest([rect, client.commit(f.ti)]).await;
        let rect = f.v1(V1::SetCursorRectangle {
            x: 10,
            y: 20,
            width: 1,
            height: 16,
        });
        assert_eq!(f.s.host_received(), [rect, f.commit_state(2)]);
        let events = f.s.guest_received();
        assert_eq!(events, f.events([V3::Done { serial: 2 }]));
        client.receive(events);
        assert!(client.may_send());

        f.s.guest([v3::disable(f.ti), client.commit(f.ti)]).await;
        let host = f.s.host_received();
        assert_eq!(host, f.disable_requests(barrier(&host), 3));
        assert_eq!(f.s.guest_received(), []);
    }

    /// The `done` that answers a commit carries the preedit the guest
    /// shows, with its cursor, as mutter's and KWin's do: a `done` applies
    /// the preedit sent since the previous one, or none, so without it the
    /// guest would drop its composition. Chromium commits after every
    /// preedit it applies.
    #[tokio::test]
    async fn a_commit_during_composition_keeps_the_preedit() {
        let mut f = Field::disabled().await;
        let mut client = DoneCountingClient::new();
        f.s.guest([v3::enable(f.ti), client.commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        client.receive(f.s.guest_received());

        f.s.host([v1::preedit_cursor(f.host, 3), f.preedit("가나", "가나")])
            .await;
        let events = f.s.guest_received();
        let composing = || preedit_at("가나", 3, 3);
        let expected = [composing(), V3::Done { serial: 1 }];
        assert_eq!(events, f.events(expected));
        client.receive(events);
        assert_eq!(client.preedit, composing());

        let rect = v3::set_cursor_rectangle(f.ti, 10, 20, 1, 16);
        f.s.guest([rect, client.commit(f.ti)]).await;
        f.s.host_received();
        let events = f.s.guest_received();
        let expected = [composing(), V3::Done { serial: 2 }];
        assert_eq!(events, f.events(expected));
        client.receive(events);
        assert!(client.may_send());
        assert_eq!(client.preedit, composing());

        // The composition goes on.
        f.s.host([f.preedit("가낙", "가낙")]).await;
        let expected = [preedit("가낙"), V3::Done { serial: 2 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A pointer press on the surface of a composition ends it before the
    /// press reaches the guest, as mutter
    /// (`meta_wayland_text_input_handle_event`) and KWin
    /// (`InputMethodEventFilter::pointerButton`) do: the guest commits the
    /// preedit it shows, and Exo, told to `reset`, drops the composition,
    /// so the next key starts a new syllable instead of joining the old one.
    #[tokio::test]
    async fn a_press_on_the_composing_surface_commits_the_preedit() {
        let mut f = Field::enabled().await;
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        // The reset goes behind a fence: Exo's events sent before it
        // handled the reset belong to the dropped composition.
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);
        f.drain(fence).await;

        // The composition ended: the release changes nothing, and the
        // input method's next commit replaces no preedit.
        let released = wl_pointer::BUTTON_STATE_RELEASED;
        f.s.host([Field::button(pointer, released)]).await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_button(pointer, released)]
        );
        f.s.host([f.commit("나")]).await;
        let expected = [commit("나"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A press that ends a composition is re-sent in its place in its host
    /// batch: the guest gets the commit transaction, then the press, then
    /// the rest of the batch in order and unchanged.
    #[tokio::test]
    async fn a_press_keeps_its_place_in_its_batch() {
        let mut f = Field::enabled().await;
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([
            Field::button(pointer, pressed),
            wl_pointer::evt::frame(pointer.host),
            wl_pointer::evt::motion(pointer.host, 510, 300, 520),
            wl_pointer::evt::frame(pointer.host),
        ])
        .await;
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        let frame = || -> (u32, AnyEvent) { (pointer.id, wl_pointer::Event::Frame {}.into()) };
        let motion = wl_pointer::Event::Motion {
            time: 510,
            surface_x: 300,
            surface_y: 520,
        };
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.extend([
            Field::guest_button(pointer, pressed),
            frame(),
            (pointer.id, motion.into()),
            frame(),
        ]);
        assert_eq!(f.s.guest_received(), expected);
        f.drain(fence).await;
    }

    /// The events Exo sent before it handled the press's `reset` still
    /// arrive, and its commits belong to the composition the press
    /// committed: the input method's own commit of the syllable, as when
    /// Space ends it just before the click, or of the syllable it became
    /// after the preedit the press committed. The guest gets neither, so
    /// no syllable reaches it twice. Commits after the reset's fence reach
    /// the guest.
    #[tokio::test]
    async fn a_commit_in_flight_at_a_press_is_dropped() {
        let mut f = Field::enabled().await;
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        f.s.host([f.preedit("각", "각")]).await;
        let expected = [preedit("각"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        let mut expected = f.events([no_preedit(), commit("각"), V3::Done { serial: 1 }]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);

        // In flight: the input method's commit of the syllable, with its
        // cursor, and of the syllable it became.
        let cursor = v1::cursor_position(f.host, 3, 3);
        f.s.host([cursor, f.commit("각"), f.commit("갂")]).await;
        assert_eq!(f.s.guest_received(), []);

        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        f.drain(fence).await;
        f.s.host([f.commit("나")]).await;
        let expected = [commit("나"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A preedit Exo sent before it handled the press's `reset` belongs to
    /// the composition the reset dropped: the guest never shows it, so a
    /// second press commits nothing, and the guest does not end up with
    /// `가간`. Preedits after the reset's fence reach the guest.
    #[tokio::test]
    async fn a_preedit_in_flight_at_a_press_is_dropped() {
        let mut f = Field::enabled().await;
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);

        // In flight: the syllable the next jamo made, with its cursor.
        f.s.host([v1::preedit_cursor(f.host, 3), f.preedit("간", "간")])
            .await;
        assert_eq!(f.s.guest_received(), []);
        f.s.host([Field::button(pointer, pressed)]).await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_button(pointer, pressed)]
        );

        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        // The dropped preedit's cursor went with it.
        f.drain(fence).await;
        f.s.host([f.preedit("나다", "나다")]).await;
        let expected = [preedit("나다"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// Through the field "나", the guest shows the preedit "가", and a press
    /// commits it behind a reset fence. Returns the pointer and the fence.
    async fn press_after_na_ga(f: &mut Field) -> (Object, u32) {
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 2 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 2 }]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        (pointer, fence)
    }

    /// A delete Exo sent before it handled the press's `reset` was made
    /// against the composition the press committed: the guest never gets
    /// it, even one counted from the cursor, which the proxy would
    /// otherwise forward at once.
    #[tokio::test]
    async fn a_delete_in_flight_at_a_press_is_dropped() {
        let mut f = Field::with_surrounding_text("나", 3, 3).await;
        let (_, fence) = press_after_na_ga(&mut f).await;
        f.s.host([f.delete_range(-3, 3)]).await;
        assert_eq!(f.s.guest_received(), []);
        f.drain(fence).await;
        f.s.host([f.delete_range(-3, 3)]).await;
        let expected = [delete(3, 0), V3::Done { serial: 2 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A preedit region Exo sent before it handled the press's `reset`
    /// would turn text before the cursor back into a composition the
    /// reset dropped: the guest never gets it.
    #[tokio::test]
    async fn a_preedit_region_in_flight_at_a_press_is_dropped() {
        let mut f = Field::with_surrounding_text("나", 3, 3).await;
        let (_, fence) = press_after_na_ga(&mut f).await;
        f.s.host([ext::set_preedit_region(f.host_ext, -3, 3)]).await;
        assert_eq!(f.s.guest_received(), []);
        f.drain(fence).await;
    }

    /// A confirmation Exo sent before it handled the press's `reset`
    /// confirms the composition the press committed. With nothing composed
    /// it would stand for the held Space, which the guest would then get
    /// as a key the input method never gave it.
    #[tokio::test]
    async fn a_confirm_in_flight_at_a_press_is_dropped() {
        let mut f = Field::typing().await;
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        let space = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: 700,
            time: 1_234,
        };
        f.s.host([f.peek(space), f.preedit("각", "각")]).await;
        assert_eq!(f.s.guest_received(), f.events([preedit("각"), DONE]));
        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        let mut expected = f.events([no_preedit(), commit("각"), DONE]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);

        f.s.host([f.confirm()]).await;
        assert_eq!(f.s.guest_received(), []);
        f.drain(fence).await;
    }

    /// The field shows the preedit "가", and a press commits it behind a
    /// reset fence. Returns the fence.
    async fn press_during_ga(f: &mut Field) -> u32 {
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        f.s.host([f.preedit("가", "가")]).await;
        assert_eq!(f.s.guest_received(), f.events([preedit("가"), DONE]));
        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        let mut expected = f.events([no_preedit(), commit("가"), DONE]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        fence
    }

    /// A focus change while a reset fence is pending deactivates behind a
    /// drain barrier, which the host answers after the fence. The fence
    /// then ends, and the next generation's composition reaches the guest.
    #[tokio::test]
    async fn a_reset_fence_ends_before_the_next_generation() {
        let mut f = Field::enabled().await;
        let fence = press_during_ga(&mut f).await;
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let drain = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(drain)]);
        f.drain(fence).await;
        f.drain(drain).await;

        f.keyboard_enter().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(2));
        f.answered(2);
        f.s.host([f.preedit("나", "나")]).await;
        let expected = [preedit("나"), V3::Done { serial: 2 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A text input destroyed while its reset fence is pending takes the
    /// fence with it: the fence's return later changes nothing, not even
    /// for a new text input that reuses the guest ID.
    #[tokio::test]
    async fn a_reset_fence_ends_with_its_text_input() {
        let mut f = Field::enabled().await;
        let fence = press_during_ga(&mut f).await;
        f.s.guest([v3::destroy(f.ti)]).await;
        let host = f.s.host_received();
        let drain = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(drain), f.ext(Ext::Destroy {})]);
        let delete_id = wl_display::Event::DeleteId { id: f.ti };
        assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);

        let ti = f.ti;
        f.create_text_input(ti).await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        f.answered(1);
        f.drain(fence).await;
        f.drain(drain).await;
        assert_eq!(f.s.guest_received(), []);
        f.s.host([f.preedit("나", "나")]).await;
        let expected = [preedit("나"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// Only a press on the surface of a composition ends it: a press with
    /// no composition, a press on another surface and a release change
    /// nothing, and the composition goes on.
    #[tokio::test]
    async fn a_press_elsewhere_or_without_a_composition_changes_nothing() {
        let mut f = Field::enabled().await;
        let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
        let (pressed, released) = (
            wl_pointer::BUTTON_STATE_PRESSED,
            wl_pointer::BUTTON_STATE_RELEASED,
        );

        // No composition.
        f.s.host([Field::button(pointer, pressed)]).await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_button(pointer, pressed)]
        );
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // A release.
        f.s.host([Field::button(pointer, released)]).await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_button(pointer, released)]
        );

        // A press on another surface, or after the pointer left.
        let other = f.s.surface().await;
        let other_pointer = f.s.pointer_on(f.seat.id, other).await;
        f.s.host([Field::button(other_pointer, pressed)]).await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_button(other_pointer, pressed)]
        );
        let leave = wl_pointer::evt::leave(pointer.host, 5, f.surface.host);
        f.s.host([leave, Field::button(pointer, pressed)]).await;
        let leave = wl_pointer::Event::Leave {
            serial: 5,
            surface: f.surface.id,
        };
        assert_eq!(
            f.s.guest_received(),
            [
                (pointer.id, leave.into()),
                Field::guest_button(pointer, pressed)
            ]
        );

        // The host was told nothing, and the composition goes on.
        assert_eq!(f.s.host_received(), []);
        f.s.host([f.commit("각")]).await;
        let expected = [no_preedit(), commit("각"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A touch-down on the surface of a composition ends it as a pointer
    /// press does, before the guest gets the down, as mutter
    /// (`meta_wayland_text_input_handle_event` on a touch-begin) and KWin
    /// (`InputMethodEventFilter::touchDown`) do.
    #[tokio::test]
    async fn a_touch_down_on_the_composing_surface_commits_the_preedit() {
        let mut f = Field::enabled().await;
        let touch = f.s.touch(f.seat.id).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([Field::down(touch, f.surface, 1)]).await;
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.push(Field::guest_down(touch, f.surface, 1));
        assert_eq!(f.s.guest_received(), expected);
        f.drain(fence).await;

        // The composition ended: the input method's next commit replaces
        // no preedit.
        f.s.host([f.commit("나")]).await;
        let expected = [commit("나"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A touch-down that ends a composition is re-sent in its place in its
    /// host batch: the guest gets the commit transaction, then the down,
    /// then the rest of the batch in order and unchanged, a second finger's
    /// down during the reset fence included.
    #[tokio::test]
    async fn a_touch_down_keeps_its_place_in_its_batch() {
        let mut f = Field::enabled().await;
        let touch = f.s.touch(f.seat.id).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        let host_frame = || wl_touch::evt::frame(touch.host);
        f.s.host([
            Field::down(touch, f.surface, 1),
            host_frame(),
            wl_touch::evt::motion(touch.host, 610, 1, 300, 520),
            Field::down(touch, f.surface, 2),
            host_frame(),
            wl_touch::evt::up(touch.host, 7, 620, 1),
            host_frame(),
        ])
        .await;
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        let frame = || -> (u32, AnyEvent) { (touch.id, wl_touch::Event::Frame {}.into()) };
        let motion = wl_touch::Event::Motion {
            time: 610,
            id: 1,
            x: 300,
            y: 520,
        };
        let up = wl_touch::Event::Up {
            serial: 7,
            time: 620,
            id: 1,
        };
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.extend([
            Field::guest_down(touch, f.surface, 1),
            frame(),
            (touch.id, motion.into()),
            Field::guest_down(touch, f.surface, 2),
            frame(),
            (touch.id, up.into()),
            frame(),
        ]);
        assert_eq!(f.s.guest_received(), expected);
        f.drain(fence).await;
    }

    /// Only a touch-down on the surface of a composition ends it: one with
    /// no composition or on another surface changes nothing, and the
    /// composition goes on.
    #[tokio::test]
    async fn a_touch_down_elsewhere_or_without_a_composition_changes_nothing() {
        let mut f = Field::enabled().await;
        let touch = f.s.touch(f.seat.id).await;

        // No composition.
        f.s.host([Field::down(touch, f.surface, 1)]).await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_down(touch, f.surface, 1)]
        );
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // A second finger, on another surface.
        let other = f.s.surface().await;
        f.s.host([Field::down(touch, other, 2)]).await;
        assert_eq!(f.s.guest_received(), [Field::guest_down(touch, other, 2)]);

        // The host was told nothing, and the composition goes on.
        assert_eq!(f.s.host_received(), []);
        f.s.host([f.commit("각")]).await;
        let expected = [no_preedit(), commit("각"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A second finger's touch-down while the reset fence of the first
    /// finger's is pending ends nothing: the preedit Exo sent before it
    /// handled the `reset` never reached the guest, so the host gets no
    /// second `reset` and the guest no second commit.
    #[tokio::test]
    async fn a_touch_down_during_a_reset_fence_sends_no_second_reset() {
        let mut f = Field::enabled().await;
        let touch = f.s.touch(f.seat.id).await;
        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([Field::down(touch, f.surface, 1)]).await;
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.push(Field::guest_down(touch, f.surface, 1));
        assert_eq!(f.s.guest_received(), expected);

        // In flight: the syllable the next jamo made. Then a second
        // finger touches down while the first stays down.
        f.s.host([f.preedit("간", "간"), Field::down(touch, f.surface, 2)])
            .await;
        assert_eq!(
            f.s.guest_received(),
            [Field::guest_down(touch, f.surface, 2)]
        );
        assert_eq!(f.s.host_received(), []);
        f.drain(fence).await;
    }

    /// A `leave` names the surface it leaves. One that names a surface
    /// other than the pointer's, as from a host that delivers a `leave`
    /// late, leaves the pointer where it is, so a press there still ends
    /// the composition.
    #[tokio::test]
    async fn a_leave_of_another_surface_keeps_the_pointer_on_its_surface() {
        let mut f = Field::enabled().await;
        let other = f.s.surface().await;
        let pointer = f.s.pointer_on(f.seat.id, other).await;
        let leave = wl_pointer::evt::leave(pointer.host, 2, other.host);
        let enter = wl_pointer::evt::enter(pointer.host, 3, f.surface.host, 0, 0);
        let late_leave = wl_pointer::evt::leave(pointer.host, 4, other.host);
        f.s.host([leave, enter, late_leave]).await;
        let leave = |serial| wl_pointer::Event::Leave {
            serial,
            surface: other.id,
        };
        let enter = wl_pointer::Event::Enter {
            serial: 3,
            surface: f.surface.id,
            surface_x: 0,
            surface_y: 0,
        };
        assert_eq!(
            f.s.guest_received(),
            [
                (pointer.id, leave(2).into()),
                (pointer.id, enter.into()),
                (pointer.id, leave(4).into()),
            ]
        );

        f.s.host([f.preedit("가", "가")]).await;
        let expected = [preedit("가"), V3::Done { serial: 1 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
        let pressed = wl_pointer::BUTTON_STATE_PRESSED;
        f.s.host([Field::button(pointer, pressed)]).await;
        let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
        expected.push(Field::guest_button(pointer, pressed));
        assert_eq!(f.s.guest_received(), expected);
        let host = f.s.host_received();
        let fence = barrier(&host);
        assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
        f.drain(fence).await;
    }

    /// An input method that replaces text deletes, then commits, each a
    /// transaction of its own. A client that waits for `done`, as Chromium
    /// does, reports its text after the first, before it has applied the
    /// second; the `done` that answers that report lets it report the text
    /// after the second at once, rather than at the input method's next
    /// event, so the next delete is measured against the text the guest
    /// shows. A delete ends a preedit, and the `done` of the next commit
    /// repeats none.
    #[tokio::test]
    async fn a_client_that_waits_for_done_reports_the_text_an_edit_leaves() {
        let mut f = Field::disabled().await;
        let mut client = DoneCountingClient::new();
        f.s.guest([
            v3::enable(f.ti),
            v3::set_surrounding_text(f.ti, "see you tom", 11, 11).unwrap(),
            client.commit(f.ti),
        ])
        .await;
        f.s.host_received();
        client.receive(f.s.guest_received());
        assert!(client.may_send());
        let report = |f: &Field, text: &str| {
            let end = text.len() as i32;
            v3::set_surrounding_text(f.ti, text, end, end).unwrap()
        };
        let done = |serial| V3::Done { serial };

        // The user accepts the suggestion "tomorrow".
        f.s.host([f.delete_range(8, 3), f.commit("tomorrow")]).await;
        let mut events = f.s.guest_received();
        let expected = [delete(3, 0), done(1), commit("tomorrow"), done(1)];
        assert_eq!(events, f.events(expected));
        let commit_transaction = events.split_off(2);
        client.receive(events);
        assert!(client.may_send());
        f.s.guest([report(&f, "see you "), client.commit(f.ti)])
            .await;
        let expected = [f.surrounding_text("see you ", 8, 8), f.commit_state(2)];
        assert_eq!(f.s.host_received(), expected);
        client.receive(commit_transaction);
        assert!(!client.may_send());
        let events = f.s.guest_received();
        assert_eq!(events, f.events([done(2)]));
        client.receive(events);
        assert!(client.may_send());
        f.s.guest([report(&f, "see you tomorrow"), client.commit(f.ti)])
            .await;
        let expected = [
            f.surrounding_text("see you tomorrow", 16, 16),
            f.commit_state(3),
        ];
        assert_eq!(f.s.host_received(), expected);
        f.answered(3);

        // The input method composes, and the caret moves.
        f.s.host([f.preedit("x", "x")]).await;
        client.receive(f.s.guest_received());
        let rect = v3::set_cursor_rectangle(f.ti, 10, 20, 1, 16);
        f.s.guest([rect, client.commit(f.ti)]).await;
        f.s.host_received();
        let events = f.s.guest_received();
        assert_eq!(events, f.events([preedit("x"), done(4)]));
        client.receive(events);

        // The input method takes the suggestion back, which is measured
        // against the text reported last.
        f.s.host([f.delete_range(8, 8)]).await;
        let events = f.s.guest_received();
        let expected = [no_preedit(), delete(8, 0), done(4)];
        assert_eq!(events, f.events(expected));
        client.receive(events);
        assert!(client.may_send());
        f.s.guest([report(&f, "see you "), client.commit(f.ti)])
            .await;
        f.s.host_received();
        f.answered(5);
    }

    /// The `done` of the guest's first commit.
    const DONE: V3 = V3::Done { serial: 1 };

    fn delete(before_length: u32, after_length: u32) -> V3 {
        V3::DeleteSurroundingText {
            before_length,
            after_length,
        }
    }

    /// The host input method's events reach the guest as text-input-v3
    /// transactions, each closed by a `done` that numbers the guest's
    /// commits, whatever serial the host sent.
    #[tokio::test]
    async fn host_ime_events_become_v3_transactions() {
        type Events = fn(&Field) -> Vec<Outgoing>;
        let korean: Events = |f| {
            vec![
                f.preedit("ㄱ", "ㄱ"),
                f.preedit("가", "가"),
                f.preedit("간", "간"),
                f.commit("가"),
                f.preedit("나", "나"),
                f.preedit("낟", "낟"),
                f.commit("나"),
                f.preedit("다", "다"),
            ]
        };
        let cases: Vec<(&str, Events, Vec<V3>)> = vec![
            (
                "a preedit",
                |f| vec![v1::preedit_string(f.host, 42, "こんにちは", "").unwrap()],
                vec![preedit("こんにちは"), DONE],
            ),
            (
                "a live preedit keeps its fallback",
                |f| vec![f.preedit("가", ""), f.preedit("간", "reset fallback")],
                vec![preedit("가"), DONE, preedit("간"), DONE],
            ),
            (
                "a preedit reset commits its fallback",
                |f| vec![f.preedit("미완성", ""), f.preedit("", "확정")],
                vec![preedit("미완성"), DONE, no_preedit(), commit("확정"), DONE],
            ),
            (
                "a reset without a preedit commits nothing",
                |f| vec![f.preedit("", "stale fallback")],
                vec![DONE],
            ),
            (
                // GTK emits `commit` for an empty commit_string too
                // (`text_input_commit_apply`, `gtk/gtkimcontextwayland.c`
                // 281-282, GTK 92348cd4), and GTK4's `GtkText` and
                // `GtkTextView` delete the selection before they insert it
                // (`gtk_text_enter_text`, `gtk/gtktext.c` 4592-4593;
                // `gtk_text_view_commit_text`, `gtk/gtktextview.c` 8779).
                "an empty commit sends no commit_string",
                |f| vec![f.commit(""), f.preedit("미완성", ""), f.commit("")],
                vec![DONE, preedit("미완성"), DONE, no_preedit(), DONE],
            ),
            (
                "a commit clears the preedit",
                |f| vec![f.preedit("미완성", ""), f.commit("確定")],
                vec![preedit("미완성"), DONE, no_preedit(), commit("確定"), DONE],
            ),
            (
                "Korean composition commits each syllable once",
                korean,
                vec![
                    preedit("ㄱ"),
                    DONE,
                    preedit("가"),
                    DONE,
                    preedit("간"),
                    DONE,
                    no_preedit(),
                    commit("가"),
                    DONE,
                    preedit("나"),
                    DONE,
                    preedit("낟"),
                    DONE,
                    no_preedit(),
                    commit("나"),
                    DONE,
                    preedit("다"),
                    DONE,
                ],
            ),
            (
                // Its preedit_cursor and preedit_styling are byte offsets
                // into it, which replacing an invalid sequence would move.
                "a preedit that is not UTF-8 is ignored",
                |f| {
                    let latin1 = respell(f.preedit("Cafe", ""), "Cafe", b"Caf\xe9");
                    vec![f.preedit("가", ""), latin1, f.preedit("가나", "")]
                },
                vec![preedit("가"), DONE, preedit("가나"), DONE],
            ),
            (
                "an ignored preedit takes its cursor and styling with it",
                |f| {
                    let latin1 = respell(f.preedit("Cafe", ""), "Cafe", b"Caf\xe9");
                    vec![
                        v1::preedit_styling(f.host, 0, 3, 6),
                        v1::preedit_cursor(f.host, 3),
                        latin1,
                        f.preedit("가나", ""),
                    ]
                },
                vec![preedit("가나"), DONE],
            ),
            (
                "a preedit region without surrounding text takes the cursor and styling with it",
                |f| {
                    vec![
                        v1::preedit_styling(f.host, 0, 3, 6),
                        v1::preedit_cursor(f.host, 3),
                        ext::set_preedit_region(f.host_ext, -3, 3),
                        f.preedit("가나", ""),
                    ]
                },
                vec![preedit("가나"), DONE],
            ),
            (
                // Nothing the proxy uses indexes committed text.
                "a commit that is not UTF-8 commits replacement characters",
                |f| vec![respell(f.commit("Cafe"), "Cafe", b"Caf\xe9")],
                vec![commit("Caf\u{fffd}"), DONE],
            ),
            (
                // Each 0xff becomes the three bytes of U+FFFD, too long for
                // the guest's message. The cursor position it takes with it
                // reaches no guest event; the state test
                // `oversized_host_text_does_not_partially_advance_state`
                // reads it.
                "a commit too long once replaced is dropped",
                |f| {
                    let text = "x".repeat(4_000);
                    let latin1 = respell(f.commit(&text), &text, &[0xff; 4_000]);
                    vec![v1::cursor_position(f.host, 1, 1), latin1, f.commit("나")]
                },
                vec![commit("나"), DONE],
            ),
            (
                "a preedit cursor is a byte offset",
                |f| vec![v1::preedit_cursor(f.host, 3), f.preedit("가나", "")],
                vec![preedit_at("가나", 3, 3), DONE],
            ),
            (
                "a selection is a byte range, which the cursor does not move",
                |f| {
                    vec![
                        v1::preedit_styling(f.host, 0, 3, 6),
                        v1::preedit_cursor(f.host, 6),
                        f.preedit("가나", ""),
                    ]
                },
                vec![preedit_at("가나", 0, 3), DONE],
            ),
            (
                "a cursor inside a character falls back to the end",
                |f| vec![v1::preedit_cursor(f.host, 1), f.preedit("가", "")],
                vec![preedit("가"), DONE],
            ),
            (
                "a selection inside a character falls back to the end",
                |f| vec![v1::preedit_styling(f.host, 1, 1, 6), f.preedit("가", "")],
                vec![preedit("가"), DONE],
            ),
            (
                "a confirm without a preedit or a held key",
                |f| vec![ext::confirm_preedit(f.host_ext, 1)],
                vec![],
            ),
            (
                "a confirm of an emptied preedit without a held key",
                |f| {
                    vec![
                        f.preedit("가", ""),
                        f.preedit("", ""),
                        ext::confirm_preedit(f.host_ext, 0),
                    ]
                },
                vec![preedit("가"), DONE, no_preedit(), DONE],
            ),
            (
                // Its selection and cursor go with it.
                "a confirm commits the preedit",
                |f| {
                    vec![
                        f.preedit("나", ""),
                        v1::preedit_styling(f.host, 0, 3, 6),
                        v1::preedit_cursor(f.host, 3),
                        ext::confirm_preedit(f.host_ext, 0),
                        f.preedit("다라", ""),
                        f.commit("다"),
                        f.commit("라"),
                    ]
                },
                vec![
                    preedit("나"),
                    DONE,
                    no_preedit(),
                    commit("나"),
                    DONE,
                    preedit_at("다라", 6, 6),
                    DONE,
                    no_preedit(),
                    commit("다"),
                    DONE,
                    commit("라"),
                    DONE,
                ],
            ),
        ];
        for (case, events, expected) in cases {
            let mut f = Field::enabled().await;
            let events = events(&f);
            f.s.host(events).await;
            assert_eq!(f.s.guest_received(), f.events(expected), "{case}");
        }
    }

    /// Exo's `delete_surrounding_text(index, length)` deletes the bytes
    /// `index..index + length` of the surrounding text, counted from its
    /// start, and no `commit_string` closes it. The guest gets it at once,
    /// in bytes before and after its cursor. A delete is dropped where v3
    /// cannot carry it to every client (a range the cursor is not in, a
    /// selection) and where the text Exo measured may not be the text the
    /// guest committed (a range that text does not have, an edit of the
    /// input method the guest has not reported since).
    #[tokio::test]
    async fn exo_deletes_become_v3_deletes_around_the_cursor() {
        type Events = fn(&Field) -> Vec<Outgoing>;
        /// The guest's surrounding text, cursor and anchor, if it has any.
        type Surrounding = Option<(&'static str, i32, i32)>;
        /// The `done` after the guest's second commit, which stated the
        /// surrounding text.
        const DONE_2: V3 = V3::Done { serial: 2 };
        let cases: Vec<(&str, Surrounding, Events, Vec<V3>)> = vec![
            (
                // The input method replaces "tom" with the suggestion.
                "an accepted suggestion deletes, then commits",
                Some(("see you tom", 11, 11)),
                |f| vec![f.delete_range(8, 3), f.commit("tomorrow")],
                vec![delete(3, 0), DONE_2, commit("tomorrow"), DONE_2],
            ),
            (
                // A long press on "e" offers its diacritics.
                "a chosen diacritic deletes, then commits",
                Some(("e", 1, 1)),
                |f| vec![f.delete_range(0, 1), f.commit("é")],
                vec![delete(1, 0), DONE_2, commit("é"), DONE_2],
            ),
            (
                // "하세요" is three syllables of three bytes each.
                "a delete alone counts UTF-8 bytes",
                Some(("안녕하세요", 15, 15)),
                |f| vec![f.delete_range(6, 9)],
                vec![delete(9, 0), DONE_2],
            ),
            (
                "a delete around the cursor",
                Some(("가나다", 3, 3)),
                |f| vec![f.delete_range(0, 6)],
                vec![delete(3, 3), DONE_2],
            ),
            (
                // Exo deletes the selection "cd" and one byte either side,
                // which v3 clients count from different places.
                "a delete with a selection is dropped",
                Some(("abcdef", 4, 2)),
                |f| vec![f.delete_range(1, 4), f.commit("x")],
                vec![commit("x"), DONE_2],
            ),
            (
                "a delete with a selection whose cursor is its start is dropped",
                Some(("abcdef", 2, 4)),
                |f| vec![f.delete_range(1, 4), f.commit("x")],
                vec![commit("x"), DONE_2],
            ),
            (
                "a range apart from the cursor is dropped, not kept",
                Some(("가나다", 9, 9)),
                |f| vec![f.delete_range(0, 3), f.commit("x")],
                vec![commit("x"), DONE_2],
            ),
            (
                "a range past the text is dropped",
                Some(("가", 3, 3)),
                |f| vec![f.delete_range(0, 6), f.commit("x")],
                vec![commit("x"), DONE_2],
            ),
            (
                "a range inside a character is dropped",
                Some(("가나", 6, 6)),
                |f| vec![f.delete_range(1, 5), f.commit("x")],
                vec![commit("x"), DONE_2],
            ),
            (
                "a delete without surrounding text is dropped",
                None,
                |f| vec![f.delete_range(0, 3), f.commit("x")],
                vec![commit("x"), DONE],
            ),
            (
                // Exo deletes only without a composition; were one shown,
                // the delete would end it, as a commit does.
                "a delete ends the preedit",
                Some(("가", 3, 3)),
                |f| vec![f.preedit("나", ""), f.delete_range(0, 3), f.confirm()],
                vec![preedit("나"), DONE_2, no_preedit(), delete(3, 0), DONE_2],
            ),
            (
                // The v1 XML's reading, relative to the cursor, which only
                // a negative index can mean.
                "a negative index counts from the cursor",
                Some(("abc", 3, 3)),
                |f| vec![f.delete_range(-1, 1)],
                vec![delete(1, 0), DONE_2],
            ),
            (
                "a negative index with a length past the cursor",
                Some(("abc", 1, 1)),
                |f| vec![f.delete_range(-1, 3)],
                vec![delete(1, 2), DONE_2],
            ),
            (
                "a negative index whose range ends before the cursor is dropped",
                Some(("abc", 3, 3)),
                |f| vec![f.delete_range(-2, 1), f.commit("x")],
                vec![commit("x"), DONE_2],
            ),
            (
                // Its range is the guest's own, around its cursor.
                "a negative index needs no report of the last edit",
                Some(("abc", 3, 3)),
                |f| vec![f.commit("X"), f.delete_range(-1, 1)],
                vec![commit("X"), DONE_2, delete(1, 0), DONE_2],
            ),
            (
                // Exo sends one when the input method deletes nothing.
                "an empty delete is not forwarded, and edits nothing",
                Some(("abc", 3, 3)),
                |f| vec![f.delete_range(3, 0), f.delete_range(2, 1)],
                vec![delete(1, 0), DONE_2],
            ),
            (
                // It sends no commit_string either (`host_ime_events_become_v3_transactions`).
                "an empty commit without a preedit edits nothing",
                Some(("abc", 3, 3)),
                |f| vec![f.commit(""), f.delete_range(2, 1)],
                vec![DONE_2, delete(1, 0), DONE_2],
            ),
            (
                // The guest gets only the preedit's clear, as from an empty
                // preedit, so it has no edit to report.
                "an empty commit over a preedit edits nothing",
                Some(("abc", 3, 3)),
                |f| vec![f.preedit("x", ""), f.commit(""), f.delete_range(2, 1)],
                vec![
                    preedit("x"),
                    DONE_2,
                    no_preedit(),
                    DONE_2,
                    delete(1, 0),
                    DONE_2,
                ],
            ),
            (
                // It commits the preedit, an edit, as a confirmation does
                // ([`an_empty_region_over_a_held_preedit_commits_it`]).
                "an empty preedit region over a preedit commits it",
                Some(("abc", 3, 3)),
                |f| {
                    vec![
                        f.preedit("x", ""),
                        ext::set_preedit_region(f.host_ext, 0, 0),
                        f.delete_range(2, 1),
                    ]
                },
                vec![preedit("x"), DONE_2, no_preedit(), commit("x"), DONE_2],
            ),
            (
                // The guest gets only a `done`, so it has no edit to report.
                "an empty preedit region edits nothing",
                Some(("abc", 3, 3)),
                |f| {
                    vec![
                        ext::set_preedit_region(f.host_ext, 0, 0),
                        f.delete_range(2, 1),
                    ]
                },
                vec![DONE_2, delete(1, 0), DONE_2],
            ),
            (
                // Exo deletes "cX" of its "abcX|def"; in the guest's
                // "abc|def", the same bytes are "c" and "d".
                "a delete after a commit the guest has not reported is dropped",
                Some(("abcdef", 3, 3)),
                |f| vec![f.commit("X"), f.delete_range(2, 2)],
                vec![commit("X"), DONE_2],
            ),
            (
                // Exo deletes "bd" of its "ab|def"; in "abc|def", the same
                // bytes are "ab".
                "a delete after a delete the guest has not reported is dropped",
                Some(("abcdef", 3, 3)),
                |f| vec![f.delete_range(2, 1), f.delete_range(1, 2)],
                vec![delete(1, 0), DONE_2],
            ),
            (
                // Exo deletes "b" of its "ab|cd"; in "a|cd", the same byte
                // is "c".
                "a delete after a confirmation the guest has not reported is dropped",
                Some(("acd", 1, 1)),
                |f| vec![f.preedit("b", ""), f.confirm(), f.delete_range(1, 1)],
                vec![preedit("b"), DONE_2, no_preedit(), commit("b"), DONE_2],
            ),
            (
                // The region "나" becomes a preedit and is cleared, so Exo
                // deletes "다" of its "가|다"; in "가나다|", the same bytes
                // are "나".
                "a delete after a preedit region the guest has not reported is dropped",
                Some(("가나다", 6, 6)),
                |f| {
                    vec![
                        ext::set_preedit_region(f.host_ext, -3, 3),
                        f.preedit("", ""),
                        f.delete_range(3, 3),
                    ]
                },
                vec![
                    delete(3, 0),
                    preedit_at("나", 3, 3),
                    DONE_2,
                    no_preedit(),
                    DONE_2,
                ],
            ),
        ];
        for (case, surrounding_text, events, expected) in cases {
            let mut f = match surrounding_text {
                Some((text, cursor, anchor)) => {
                    Field::with_surrounding_text(text, cursor, anchor).await
                }
                None => Field::enabled().await,
            };
            let events = events(&f);
            f.s.host(events).await;
            assert_eq!(f.s.guest_received(), f.events(expected), "{case}");
        }
    }

    /// A v1 delete's index is Exo's, counted from the start of the text,
    /// unless it is negative, which only the v1 XML's cursor-relative
    /// reading can mean; either way the range must contain the cursor and
    /// lie on character boundaries.
    #[test]
    fn delete_lengths_read_both_conventions() {
        /// A delete's surrounding text, cursor, index and length.
        type Delete = (&'static str, i32, i32, u32);
        /// The lengths before and after the cursor, if any.
        type Lengths = Option<(u32, u32)>;
        let cases: [(&str, Delete, Lengths); 12] = [
            (
                "absolute, before the cursor",
                ("abcdef", 3, 1, 2),
                Some((2, 0)),
            ),
            (
                "absolute, from the cursor",
                ("abcdef", 3, 3, 2),
                Some((0, 2)),
            ),
            (
                "absolute, around the cursor",
                ("abcdef", 3, 1, 4),
                Some((2, 2)),
            ),
            ("absolute, apart from the cursor", ("abcdef", 3, 0, 2), None),
            ("absolute, past the text", ("abc", 3, 1, 3), None),
            ("absolute, inside a character", ("가나", 6, 1, 5), None),
            (
                "relative, before the cursor",
                ("abc", 3, -1, 1),
                Some((1, 0)),
            ),
            (
                "relative, ending before the cursor",
                ("abc", 3, -2, 1),
                None,
            ),
            ("relative, before the text", ("abc", 1, -2, 2), None),
            ("empty, at the cursor", ("abc", 3, 3, 0), Some((0, 0))),
            (
                "overflowing a relative index",
                ("abc", 3, i32::MIN, u32::MAX),
                None,
            ),
            (
                "overflowing a length",
                ("abc", i32::MAX, i32::MAX, u32::MAX),
                None,
            ),
        ];
        for (case, (text, cursor, index, length), expected) in cases {
            assert_eq!(
                delete_lengths(text, cursor, index, length),
                expected,
                "{case}"
            );
        }
    }

    /// A delete whose range is not around the cursor is lost, which only
    /// the log shows, so the first per text input is a warning; one that
    /// waits for the guest's report is expected, and is not.
    #[tokio::test]
    async fn an_unconvertible_delete_is_warned_of_once() {
        let mut f = Field::with_surrounding_text("abc", 3, 3).await;
        // Only the log shows the warning, so read the flag that makes it
        // once.
        let warned = |f: &mut Field| f.state().warned_for_test().1;
        f.s.host([f.commit("X"), f.delete_range(2, 2)]).await;
        let done = |serial| V3::Done { serial };
        assert_eq!(f.s.guest_received(), f.events([commit("X"), done(2)]));
        assert!(!warned(&mut f));

        f.s.guest([
            v3::set_surrounding_text(f.ti, "abcX", 4, 4).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let expected = [f.surrounding_text("abcX", 4, 4), f.commit_state(3)];
        assert_eq!(f.s.host_received(), expected);
        f.answered(3);
        f.s.host([f.delete_range(0, 1)]).await;
        assert_eq!(f.s.guest_received(), []);
        assert!(warned(&mut f));
    }

    /// After an edit of the input method, Exo measures deletes against
    /// text the guest has not committed yet; a guest commit that states its
    /// surrounding text, and only such a commit, catches the proxy up.
    #[tokio::test]
    async fn a_delete_waits_for_the_guest_to_report_the_input_methods_edits() {
        let mut f = Field::with_surrounding_text("abcdef", 3, 3).await;
        f.s.host([f.commit("X"), f.delete_range(2, 2)]).await;
        let done = |serial| V3::Done { serial };
        assert_eq!(f.s.guest_received(), f.events([commit("X"), done(2)]));

        // A commit without surrounding text reports nothing.
        f.s.guest([v3::set_cursor_rectangle(f.ti, 1, 2, 3, 4), v3::commit(f.ti)])
            .await;
        let rectangle = f.v1(V1::SetCursorRectangle {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });
        assert_eq!(f.s.host_received(), [rectangle, f.commit_state(3)]);
        f.answered(3);
        f.s.host([f.delete_range(2, 2)]).await;
        assert_eq!(f.s.guest_received(), []);

        f.s.guest([
            v3::set_surrounding_text(f.ti, "abcXdef", 4, 4).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let expected = [f.surrounding_text("abcXdef", 4, 4), f.commit_state(4)];
        assert_eq!(f.s.host_received(), expected);
        f.answered(4);
        f.s.host([f.delete_range(2, 2)]).await;
        assert_eq!(f.s.guest_received(), f.events([delete(2, 0), done(4)]));
    }

    /// A preedit the host clears with a reset commit reaches the guest as
    /// that commit, an edit like any other: deletes wait until the guest
    /// reports it.
    #[tokio::test]
    async fn a_reset_commit_waits_for_the_guest_to_report_it() {
        let mut f = Field::with_surrounding_text("abc", 3, 3).await;
        f.s.host([f.preedit("가", "가"), f.preedit("", "가")]).await;
        let done = || V3::Done { serial: 2 };
        let expected = [preedit("가"), done(), no_preedit(), commit("가"), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([f.delete_range(2, 1)]).await;
        assert_eq!(f.s.guest_received(), []);
    }

    /// An empty preedit reaches the guest only to clear the one it shows.
    /// Over none, the transaction is its `done` alone, since clients tell
    /// an empty preedit from none:
    /// - GTK emits `preedit-start` for an empty preedit over none
    ///   (`text_input_preedit_apply`, `gtk/gtkimcontextwayland.c` 241-244,
    ///   GTK 92348cd4), on which GTK4's `GtkText` deletes the selection
    ///   (`gtk_text_preedit_start_cb`, `gtk/gtktext.c` 4491), as does
    ///   `GtkTextView`.
    /// - Chromium applies one that differs from the preedit it last
    ///   applied, as one with a hidden cursor does
    ///   (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc` 466,
    ///   revision ffb4f13519), and an empty composition over none deletes
    ///   the selection (`UpdateCompositionIfTextSelected`,
    ///   `ui/base/ime/linux/input_method_auralinux.cc` 325-328).
    ///
    /// Either way the user's selected text would go.
    #[tokio::test]
    async fn an_empty_preedit_reaches_only_a_guest_that_shows_one() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 2).await;
        let done = || V3::Done { serial: 2 };
        f.s.host([
            f.preedit("", ""),
            f.preedit("", "stale fallback"),
            v1::preedit_cursor(f.host, -1),
            f.preedit("", ""),
        ])
        .await;
        assert_eq!(f.s.guest_received(), f.events([done(), done(), done()]));

        f.s.host([f.preedit("가", ""), f.preedit("", "")]).await;
        let expected = [preedit("가"), done(), no_preedit(), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// An empty preedit, which clears the one the guest shows, always has
    /// the cursor `(0, 0)`, even after the host hid the cursor. Chromium
    /// keeps the last preedit it applied, cursor included, and applies a
    /// `done`'s preedit only if it differs, an absent one counting as
    /// `("", 0, 0)` (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc`
    /// 466, `zwp_text_input_v3.h` 139-140, revision ffb4f13519). After
    /// `("", -1, -1)`, the next `done` without a preedit would apply an
    /// empty composition again, which deletes the selection
    /// (`UpdateCompositionIfTextSelected`,
    /// `ui/base/ime/linux/input_method_auralinux.cc` 325-328).
    #[tokio::test]
    async fn an_empty_preedit_has_no_hidden_cursor() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 2).await;
        let done = || V3::Done { serial: 2 };
        let expected = f.events([preedit("가"), done(), no_preedit(), done()]);
        f.s.host([
            f.preedit("가", ""),
            v1::preedit_cursor(f.host, -1),
            f.preedit("", ""),
        ])
        .await;
        assert_eq!(f.s.guest_received(), expected);

        // An empty region clears a preedit Exo no longer holds the same
        // way: a region's, with its cursor hidden, whose bytes the guest
        // has reported without.
        f.s.host([
            v1::preedit_cursor(f.host, -1),
            ext::set_preedit_region(f.host_ext, -1, 1),
        ])
        .await;
        let expected = [delete(1, 0), preedit_at("d", -1, -1), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abcef", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let expected = [f.surrounding_text("abcef", 3, 3), f.commit_state(3)];
        assert_eq!(f.s.host_received(), expected);
        let done = || V3::Done { serial: 3 };
        let expected = [preedit_at("d", -1, -1), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([ext::set_preedit_region(f.host_ext, 0, 0)]).await;
        assert_eq!(f.s.guest_received(), f.events([no_preedit(), done()]));
    }

    /// An empty preedit region deletes nothing, so it sends no
    /// `delete_surrounding_text`, only the empty preedit that clears a
    /// shown one Exo no longer holds
    /// ([`an_empty_preedit_reaches_only_a_guest_that_shows_one`],
    /// [`an_empty_preedit_has_no_hidden_cursor`]); one over a preedit Exo
    /// holds commits it
    /// ([`an_empty_region_over_a_held_preedit_commits_it`]). Chromium
    /// applies an empty delete too (`ZwpTextInputV3Impl::OnDone`,
    /// `ui/ozone/platform/wayland/host/zwp_text_input_v3.cc` 440, revision
    /// ffb4f13519), and while nothing is composed it deletes the selection
    /// (`InputMethodAuraLinux::OnDeleteSurroundingText`,
    /// `ui/base/ime/linux/input_method_auralinux.cc` 518): the user's
    /// selected text would go.
    #[tokio::test]
    async fn an_empty_preedit_region_sends_no_delete() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 2).await;
        let done = || V3::Done { serial: 2 };
        f.s.host([ext::set_preedit_region(f.host_ext, 0, 0)]).await;
        assert_eq!(f.s.guest_received(), f.events([done()]));
    }

    /// A region over the preedit an earlier region made deletes only the
    /// text the guest still has. Until the guest reports its text, the
    /// surrounding text it committed still holds the bytes the earlier
    /// region took for the preedit, which the guest no longer has, so they
    /// are not deleted again. A region that does not cover them is dropped.
    /// Once the guest reports, its text is measured as it is.
    #[tokio::test]
    async fn a_region_over_a_region_deletes_only_what_the_guest_has() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
        let done = |serial| V3::Done { serial };
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1)]).await;
        let expected = [delete(1, 0), preedit_at("d", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // The guest's text is "abc|ef": of "cd", only "c" is left.
        f.s.host([ext::set_preedit_region(f.host_ext, -2, 2)]).await;
        let expected = [no_preedit(), delete(1, 0), preedit_at("cd", 2, 2), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // "ef" does not cover the shown "cd".
        f.s.host([ext::set_preedit_region(f.host_ext, 0, 2)]).await;
        assert_eq!(f.s.guest_received(), []);

        // The guest reports its text "ab|ef", which no longer holds "cd".
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abef", 2, 2).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let expected = [f.surrounding_text("abef", 2, 2), f.commit_state(3)];
        assert_eq!(f.s.host_received(), expected);
        let expected = [preedit_at("cd", 2, 2), done(3)];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1)]).await;
        let expected = [no_preedit(), delete(1, 0), preedit_at("b", 1, 1), done(3)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A host `preedit_string` after a region replaces the preedit but not
    /// what the guest lost: the bytes the region took stay out of its text
    /// until it reports, so a later region still deletes only the rest.
    /// Exo holds the new preedit in place of those bytes, so the region
    /// takes it.
    #[tokio::test]
    async fn a_region_after_a_host_preedit_deletes_only_what_the_guest_has() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
        let done = |serial| V3::Done { serial };
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1)]).await;
        let expected = [delete(1, 0), preedit_at("d", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([f.preedit("x", "")]).await;
        let _ = f.s.guest_received();

        // The guest's text is "abc|ef", and Exo's "abcx|ef": of "cx", only
        // "c" is in the guest's.
        f.s.host([ext::set_preedit_region(f.host_ext, -2, 2)]).await;
        let expected = [no_preedit(), delete(1, 0), preedit_at("cx", 2, 2), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A preedit cleared with a reset commit gives the guest the region's
    /// bytes back as text, as a commit does, so a later region deletes
    /// them again.
    #[tokio::test]
    async fn a_region_after_a_reset_commit_deletes_the_bytes_it_gave_back() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
        let done = |serial| V3::Done { serial };
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1)]).await;
        let expected = [delete(1, 0), preedit_at("d", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // The guest's text is "abcd|ef" again.
        f.s.host([f.preedit("", "d")]).await;
        let expected = [no_preedit(), commit("d"), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([ext::set_preedit_region(f.host_ext, -2, 2)]).await;
        let expected = [delete(2, 0), preedit_at("cd", 2, 2), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// Exo holds the preedit the guest shows as text at the guest's
    /// cursor, with its own cursor where the preedit's was, and measures a
    /// region from there (`SurroundingTextTracker::OnSetCompositionText`,
    /// `TextInput::SetCompositionFromExistingText`). A region over a host
    /// preedit takes it with the committed bytes around it and deletes
    /// only those, which the DST found (seed 1008182): with "a1b|" and "b"
    /// shown, region -2+2 is "bb", not "1b".
    #[tokio::test]
    async fn a_region_over_a_host_preedit_is_measured_from_exos_cursor() {
        let mut f = Field::with_surrounding_text("a1b", 3, 3).await;
        let done = |serial| V3::Done { serial };
        f.s.host([v1::preedit_cursor(f.host, 1), f.preedit("b", "b")])
            .await;
        let expected = [preedit_at("b", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([ext::set_preedit_region(f.host_ext, -2, 2)]).await;
        let expected = [no_preedit(), delete(1, 0), preedit_at("bb", 2, 2), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A non-empty region that does not cover the host preedit the guest
    /// shows is dropped: Exo would leave the rest of that preedit as text,
    /// which only a commit after the region's delete could give the guest.
    /// One that covers it on both sides deletes the bytes beyond it on
    /// each.
    #[tokio::test]
    async fn a_region_that_does_not_cover_a_host_preedit_is_dropped() {
        let mut f = Field::with_surrounding_text("abc", 1, 1).await;
        let done = |serial| V3::Done { serial };
        // Exo holds "ax|bc".
        f.s.host([v1::preedit_cursor(f.host, 1), f.preedit("x", "x")])
            .await;
        let expected = [preedit_at("x", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        for (index, length) in [(-2, 1), (0, 1)] {
            f.s.host([ext::set_preedit_region(f.host_ext, index, length)])
                .await;
            assert_eq!(f.s.guest_received(), [], "region {index}+{length}");
        }
        f.s.host([ext::set_preedit_region(f.host_ext, -2, 3)]).await;
        let expected = [no_preedit(), delete(1, 1), preedit_at("axb", 2, 2), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// An empty region at Exo's cursor ends Exo's composition and leaves
    /// the preedit it held as text: Exo sends `set_preedit_region(0, 0)`
    /// for an empty range (`zwp_text_input_manager.cc`
    /// `SetCompositionFromExistingText`, which nothing before it filters),
    /// and its tracker keeps the text with an empty composition
    /// (`SurroundingTextTracker::OnSetCompositionFromExistingText`). So the
    /// guest commits the preedit, a host preedit or a region's, which gives
    /// the region's bytes back. With Exo's cursor inside the preedit, where
    /// no commit leaves the guest's, the region is dropped.
    #[tokio::test]
    async fn an_empty_region_over_a_held_preedit_commits_it() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
        let done = |serial| V3::Done { serial };
        let empty = || ext::set_preedit_region(f.host_ext, 0, 0);
        f.s.host([f.preedit("x", "x"), empty()]).await;
        let expected = [preedit("x"), done(2), no_preedit(), commit("x"), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // A region's preedit, "d" of "abcd|ef".
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1), empty()])
            .await;
        let expected = [
            delete(1, 0),
            preedit_at("d", 1, 1),
            done(2),
            no_preedit(),
            commit("d"),
            done(2),
        ];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([ext::set_preedit_region(f.host_ext, -2, 2)]).await;
        let expected = [delete(2, 0), preedit_at("cd", 2, 2), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([
            v1::preedit_cursor(f.host, 1),
            f.preedit("xy", "xy"),
            empty(),
        ])
        .await;
        let expected = [preedit_at("xy", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A region equal to the shown one deletes nothing more: only the
    /// preedit is sent again.
    #[tokio::test]
    async fn a_region_equal_to_the_shown_one_sends_only_the_preedit() {
        let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
        let done = |serial| V3::Done { serial };
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1)]).await;
        let expected = [delete(1, 0), preedit_at("d", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([ext::set_preedit_region(f.host_ext, -1, 1)]).await;
        let expected = [preedit_at("d", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// While the guest shows the preedit a region made and has not
    /// reported, an empty region and a region that does not cover the shown
    /// one on both sides are dropped.
    #[tokio::test]
    async fn a_region_that_does_not_cover_the_shown_one_is_dropped() {
        let mut f = Field::with_surrounding_text("abcdef", 3, 3).await;
        let done = |serial| V3::Done { serial };
        // "c" before the cursor and "d" after it.
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 2)]).await;
        let expected = [delete(1, 1), preedit_at("cd", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        for (index, length) in [(0, 0), (-2, 2), (0, 2), (-1, 1)] {
            f.s.host([ext::set_preedit_region(f.host_ext, index, length)])
                .await;
            assert_eq!(f.s.guest_received(), [], "region {index}+{length}");
        }
    }

    /// A region over a region whose bytes lie on both sides of the cursor
    /// deletes the bytes beyond the shown ones on both sides.
    #[tokio::test]
    async fn a_region_over_a_region_deletes_the_rest_on_both_sides() {
        let mut f = Field::with_surrounding_text("abcdef", 3, 3).await;
        let done = |serial| V3::Done { serial };
        f.s.host([ext::set_preedit_region(f.host_ext, -1, 2)]).await;
        let expected = [delete(1, 1), preedit_at("cd", 1, 1), done(2)];
        assert_eq!(f.s.guest_received(), f.events(expected));

        f.s.host([ext::set_preedit_region(f.host_ext, -2, 4)]).await;
        let expected = [
            no_preedit(),
            delete(1, 1),
            preedit_at("bcde", 2, 2),
            done(2),
        ];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A preedit region makes committed surrounding text around the cursor
    /// the preedit; text the guest has not committed, or a region apart
    /// from the cursor, cannot be.
    #[tokio::test]
    async fn on_set_preedit_region_translates_correctly() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::set_surrounding_text(f.ti, "가나다", 6, 6).unwrap()])
            .await;
        f.s.host([ext::set_preedit_region(f.host_ext, -3, 3)]).await;
        assert_eq!(f.s.guest_received(), []);

        f.s.guest([v3::commit(f.ti)]).await;
        let mut expected = vec![f.surrounding_text("가나다", 6, 6)];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);
        f.answered(2);
        // A region apart from the cursor takes the pending cursor with it.
        f.s.host([
            v1::preedit_cursor(f.host, 0),
            ext::set_preedit_region(f.host_ext, -6, 3),
        ])
        .await;
        assert_eq!(f.s.guest_received(), []);

        f.s.host([ext::set_preedit_region(f.host_ext, -3, 3)]).await;
        let done = || V3::Done { serial: 2 };
        let expected = [delete(3, 0), preedit_at("나", 3, 3), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
        // The region is the preedit now.
        f.s.host([f.commit("나")]).await;
        let expected = [no_preedit(), commit("나"), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // A pending preedit cursor, or selection, places the region's.
        f.s.host([
            v1::preedit_cursor(f.host, 0),
            ext::set_preedit_region(f.host_ext, -3, 3),
        ])
        .await;
        let expected = [delete(3, 0), preedit_at("나", 0, 0), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
        // A region over the shown preedit clears it before its delete:
        // SDL3 applies each event as it arrives and turns the delete into
        // a Backspace (`text_input_delete_surrounding_text`,
        // `src/video/wayland/SDL_waylandevents.c` 2683-2710, main
        // 1fc62260fc81), which its `testime` ignores while a preedit shows
        // (`test/testime.c` 1084-1086). The guest's text is "가|다" now,
        // so of the region "가나" it deletes only "가"
        // ([`a_region_over_a_region_deletes_only_what_the_guest_has`]).
        f.s.host([
            v1::preedit_styling(f.host, 0, 3, 6),
            ext::set_preedit_region(f.host_ext, -6, 6),
        ])
        .await;
        let expected = [no_preedit(), delete(3, 0), preedit_at("가나", 0, 3), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // The `done` of the guest's next commit repeats the region's
        // preedit ([`answer_guest_commit`]).
        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.commit_state(3)]);
        let expected = [preedit_at("가나", 0, 3), V3::Done { serial: 3 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// Each commit_state carries the guest's commit count, which numbers
    /// every `done` until the next commit, however many host events with
    /// whatever serials it answers.
    #[tokio::test]
    async fn guest_commit_count_is_sent_to_v1_and_used_by_v3_done() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);
        f.answered(2);

        f.s.host([
            v1::preedit_string(f.host, 2942, "한", "").unwrap(),
            v1::preedit_string(f.host, 2944, "한글", "").unwrap(),
            v1::commit_string(f.host, 2945, "한글").unwrap(),
        ])
        .await;
        let done = || V3::Done { serial: 2 };
        assert_eq!(
            f.s.guest_received(),
            f.events([
                preedit("한"),
                done(),
                preedit("한글"),
                done(),
                no_preedit(),
                commit("한글"),
                done(),
            ])
        );
    }

    /// Events of a host text input that is not active belong to no guest
    /// generation: none reaches the guest or edits its next transaction.
    #[tokio::test]
    async fn inactive_host_ime_events_are_ignored() {
        // The guest enabled the field, which its released seat cannot
        // activate.
        let mut f = Field::disabled().await;
        f.release_seat().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.reset(1, Reset::default()));
        f.answered(1);
        f.s.host([
            f.preedit("가", ""),
            f.delete_range(0, 3),
            f.commit("가"),
            v1::keysym(
                f.host,
                0,
                0,
                u32::from(b'a'),
                wl_keyboard::KEY_STATE_PRESSED,
                0,
            ),
        ])
        .await;
        assert_eq!(f.s.guest_received(), []);

        // A new seat activates it.
        let host = f.bind_seat_again().await;
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, 0), false));
        expected.push(f.commit_state(1));
        assert_eq!(host, expected);
        f.s.host([f.commit("나")]).await;
        assert_eq!(f.s.guest_received(), f.events([commit("나"), DONE]));
    }

    /// The key an empty confirmation recovers reaches the guest as a
    /// press and a release that both carry the serial and time of the
    /// held key's latest `peek_key`: the serial Exo gave the key event its
    /// input method consumed.
    #[tokio::test]
    async fn a_recovered_key_carries_the_latest_peek_serial() {
        let mut f = Field::typing().await;
        let space = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: 700,
            time: 1_234,
        };
        f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
            .await;
        let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));

        let repeat = Stroke {
            state: REPEATED,
            serial: 705,
            time: 1_300,
            ..space
        };
        for held in [space, repeat] {
            if held.state == REPEATED {
                f.s.host([f.peek(held)]).await;
            }
            f.s.host([f.confirm()]).await;
            f.answer_recovery_syncs().await;
            assert_eq!(f.s.guest_received(), f.recovered(held), "{held:?}");
        }
    }

    /// At extension v11 Exo confirms before every press its input method
    /// does not handle (E13), and sends the press's peek and key after the
    /// confirmation (E8), the modifiers it carries first. While Backspace,
    /// which the input method took, is held, that confirmation is the new
    /// press's, not a repeat of Backspace: the guest gets the new key and
    /// no Backspace (DST seed 456, shrunk). The press ends the Backspace
    /// lease.
    #[tokio::test]
    async fn a_confirmation_before_an_unhandled_press_recovers_no_held_key() {
        for shifted in [false, true] {
            let mut f = Field::typing().await;
            f.lease_backspace_repeat().await;
            let space = Stroke {
                key: SPACE,
                state: PRESSED,
                serial: 701,
                time: 1_300,
            };
            let mut events = vec![f.confirm()];
            let mut expected = Vec::new();
            if shifted {
                let shift = wl_keyboard::evt::modifiers(f.keyboard.host, 701, 1, 0, 0, 0);
                events.push(shift);
                let shift = wl_keyboard::Event::Modifiers {
                    serial: 701,
                    mods_depressed: 1,
                    mods_latched: 0,
                    mods_locked: 0,
                    group: 0,
                };
                expected.push((f.keyboard.id, shift.into()));
            }
            events.extend([f.peek(space), f.key(space)]);
            f.s.host(events).await;
            expected.push(f.guest_key(space));
            assert_eq!(f.s.guest_received(), expected, "shifted: {shifted}");
            let host = f.s.host_received();
            let callback = barrier(&host);
            assert_eq!(host, [sync(callback), f.ack(701, true)]);
            f.s.host([wl_callback::evt::done(callback, 0)]).await;
            assert_eq!(f.s.guest_received(), [], "shifted: {shifted}");
            assert_eq!(f.repeat_owner(BACKSPACE), None);
        }
    }

    /// An empty confirmation while Backspace, which the input method took,
    /// is held, then the peek of a press the input method handles and
    /// that press's text: the confirmation was Backspace's autorepeat. The
    /// guest gets Backspace before the new press's text.
    #[tokio::test]
    async fn a_confirmation_before_a_handled_press_recovers_the_held_key() {
        let mut f = Field::typing().await;
        let backspace = f.lease_backspace_repeat().await;
        let r = Stroke {
            key: 19,
            state: PRESSED,
            serial: 701,
            time: 1_300,
        };
        f.s.host([f.confirm(), f.peek(r), f.preedit("ㄱ", "")])
            .await;
        let mut expected = f.recovered(backspace);
        expected.extend(f.events([preedit("ㄱ"), DONE]));
        assert_eq!(f.s.guest_received(), expected);
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), []);
    }

    /// Exo confirms as it detaches the input method on its own (E6), as
    /// when the input method's process restarts, then sends `leave` (H2).
    /// That confirmation is the detach's, not a repeat of the held Space,
    /// which the input method took. The key events in flight die with the
    /// input method, so a held key's release may never come: Space is not
    /// recovered again, but its next press is.
    #[tokio::test]
    async fn an_input_method_detach_ends_the_recovery_of_held_keys() {
        let mut f = Field::typing().await;
        let space = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: 700,
            time: 1_234,
        };
        f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
            .await;
        let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([f.confirm(), v1::leave(f.host)]).await;
        assert_eq!(f.s.guest_received(), []);
        let callback = barrier(&f.s.host_received());
        let answer = wl_callback::evt::done(callback, 0);
        f.s.host([answer, v1::enter(f.host, f.surface.host)]).await;
        assert_eq!(f.s.guest_received(), []);

        f.s.host([f.confirm()]).await;
        assert_eq!(f.s.host_received(), [], "no recovery waits");
        assert_eq!(f.s.guest_received(), []);

        let release = Stroke {
            state: RELEASED,
            serial: 701,
            ..space
        };
        let again = Stroke {
            serial: 702,
            time: 1_400,
            ..space
        };
        f.s.host([f.peek(release), f.peek(again), f.confirm()])
            .await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(again));
    }

    /// Exo's own detach (H2) ends only the recovery of the keys held
    /// across it, which stay the guest's keys: the keysym of `1` reaches
    /// the guest. Their releases died with the input method, so a
    /// `peek_key` press of a key still held is its next press, which an
    /// empty confirmation recovers.
    #[tokio::test]
    async fn a_key_held_across_a_detach_is_typed_and_recovered_again() {
        const KEY_1: u32 = 2;
        let mut f = Field::typing().await;
        let one = Stroke {
            key: KEY_1,
            state: PRESSED,
            serial: 700,
            time: 1_234,
        };
        let space = Stroke {
            key: SPACE,
            serial: 701,
            time: 1_240,
            ..one
        };
        f.s.host([f.peek(one), f.peek(space), f.confirm(), v1::leave(f.host)])
            .await;
        let callback = barrier(&f.s.host_received());
        let answer = wl_callback::evt::done(callback, 0);
        f.s.host([answer, v1::enter(f.host, f.surface.host)]).await;
        assert_eq!(f.s.guest_received(), []);

        let one_again = Stroke {
            serial: 702,
            time: 1_400,
            ..one
        };
        f.s.host([f.peek(one_again), f.keysym(u32::from(b'1'), one_again)])
            .await;
        assert_eq!(f.s.guest_received(), [f.guest_key(one_again)]);

        let space_again = Stroke {
            serial: 703,
            time: 1_500,
            ..space
        };
        f.s.host([f.peek(space_again), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(space_again));
    }

    /// A recovery that waits goes only to a text input that still passes
    /// the gate: once the guest disabled or destroyed it, or the key's
    /// keyboard, the host's next event recovers nothing.
    #[tokio::test]
    async fn a_waiting_recovery_ends_with_its_text_input_or_keyboard() {
        #[derive(Clone, Copy, Debug)]
        enum End {
            Disable,
            Destroy,
            ReleaseKeyboard,
        }
        for end in [End::Disable, End::Destroy, End::ReleaseKeyboard] {
            let mut f = Field::typing().await;
            f.lease_backspace_repeat().await;
            f.s.host([f.confirm()]).await;
            let callback = barrier(&f.s.host_received());
            let request = match end {
                End::Disable => v3::disable(f.ti),
                End::Destroy => v3::destroy(f.ti),
                End::ReleaseKeyboard => wl_keyboard::req::release(f.keyboard.id),
            };
            f.s.guest([request]).await;
            if !matches!(end, End::Destroy) {
                f.s.guest([v3::commit(f.ti)]).await;
            }
            f.s.host_received();
            f.s.guest_received();
            f.s.host([wl_callback::evt::done(callback, 0)]).await;
            assert_eq!(f.s.guest_received(), [], "{end:?}");
        }
    }

    // Traces of real IME and key bugs, step by step.

    /// Exo can give a key's peek release the serial of its press. The
    /// release still ends the press, so that a next press with that
    /// serial starts a generation an empty confirmation recovers.
    #[tokio::test]
    async fn a_peek_release_with_the_press_serial_ends_the_press() {
        let mut f = Field::typing().await;
        let press = Stroke {
            key: BACKSPACE,
            state: PRESSED,
            serial: 18_857,
            time: 100,
        };
        let release = Stroke {
            state: RELEASED,
            time: 110,
            ..press
        };
        f.s.host([f.peek(press), f.peek(release)]).await;
        assert!(!f.held(BACKSPACE));

        let again = Stroke { time: 120, ..press };
        f.s.host([f.peek(again), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(again));
        assert_eq!(f.repeat_owner(BACKSPACE), Some(f.ti));
    }

    /// A sibling text input's rejected enable, or its destroy, whether it
    /// ever committed or not, ends no repeat lease of the enabled one,
    /// whose empty confirmations go on recovering the held Backspace.
    #[tokio::test]
    async fn a_sibling_text_input_keeps_the_active_repeat_lease() {
        let mut f = Field::typing().await;
        let backspace = f.lease_backspace_repeat().await;
        f.s.host([f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(backspace));

        // A sibling whose enable is rejected, then one that never commits.
        let field = f.text_input();
        for enables in [true, false] {
            let id = f.s.new_id();
            // Creating a text input makes the fixture drive it.
            f.create_text_input(id).await;
            let sibling = f.text_input();
            if enables {
                f.s.guest([v3::enable(sibling.ti), v3::commit(sibling.ti)])
                    .await;
                assert_eq!(f.s.host_received(), f.reset(1, Reset::default()));
            }
            f.s.guest([v3::destroy(sibling.ti)]).await;
            assert_eq!(f.s.host_received(), [f.ext(Ext::Destroy {})]);
            let deleted = wl_display::Event::DeleteId { id: sibling.ti };
            assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        }
        f.select(field);

        f.s.host([f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(backspace));
    }

    /// A text input's destroy ends its repeat lease, which a new text
    /// input under its ID does not inherit; the next physical press can
    /// lease the new one.
    #[tokio::test]
    async fn a_text_input_recreated_under_its_id_does_not_inherit_the_repeat_lease() {
        let mut f = Field::typing().await;
        f.lease_backspace_repeat().await;
        f.s.guest([v3::destroy(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(
            host,
            [f.deactivate(), sync(callback), f.ext(Ext::Destroy {})]
        );
        let deleted = wl_display::Event::DeleteId { id: f.ti };
        assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        assert_eq!(f.repeat_owner(BACKSPACE), None);

        let old_host = f.host;
        f.create_text_input(f.ti).await;
        assert_ne!(f.host, old_host);
        assert_eq!(f.repeat_owner(BACKSPACE), None);
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        f.answered(1);

        let release = Stroke {
            key: BACKSPACE,
            state: RELEASED,
            serial: 701,
            time: 1_300,
        };
        let press = Stroke {
            state: PRESSED,
            serial: 702,
            time: 1_400,
            ..release
        };
        f.s.host([
            f.peek(release),
            f.peek(press),
            f.preedit("나", ""),
            f.preedit("", ""),
        ])
        .await;
        let expected = [preedit("나"), DONE, no_preedit(), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        assert_eq!(f.repeat_owner(BACKSPACE), Some(f.ti));
    }

    /// Held Space after a Korean commit, then held Backspace over an
    /// emptied preedit, each held twice. Every empty confirmation while
    /// the key is held stands for one press and release, the key's other
    /// channels (keysyms and the delayed `wl_keyboard.key`) duplicate none,
    /// and no confirmation after the release recovers it.
    #[tokio::test]
    async fn held_keys_repeat_through_confirmations_after_korean_input() {
        use xkbcommon::xkb::keysyms::{KEY_BackSpace, KEY_space};
        let mut f = Field::typing().await;

        // The input method commits the syllable with Space, whose keysyms
        // reach the proxy before the delayed wl_keyboard press: this
        // overlap once ended held Space after its first commit.
        let space = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: 700,
            time: 1_234,
        };
        f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
            .await;
        let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        let keysym_release = Stroke {
            state: RELEASED,
            serial: 701,
            time: 1_235,
            ..space
        };
        f.s.host([
            f.keysym(KEY_space, space),
            f.keysym(KEY_space, keysym_release),
            f.key(space),
        ])
        .await;
        assert_eq!(
            f.s.guest_received(),
            [f.guest_key(space), f.guest_key(keysym_release)]
        );
        assert_eq!(f.s.host_received(), [f.ack(700, false)]);

        f.s.host([f.confirm(), f.confirm(), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        let mut expected = f.recovered(space);
        expected.extend(f.recovered(space));
        expected.extend(f.recovered(space));
        assert_eq!(f.s.guest_received(), expected);

        let release = Stroke {
            state: RELEASED,
            serial: 701,
            time: 1_300,
            ..space
        };
        f.s.host([f.peek(release), f.confirm()]).await;
        assert_eq!(f.s.guest_received(), []);

        // A second hold starts a new generation, which repeats the same:
        // nothing of the first leaks across its release.
        let space = Stroke {
            serial: 702,
            time: 1_400,
            ..space
        };
        f.s.host([f.peek(space), f.preedit("나", ""), f.commit("나 ")])
            .await;
        let expected = [preedit("나"), DONE, no_preedit(), commit("나 "), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([f.confirm(), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        let mut expected = f.recovered(space);
        expected.extend(f.recovered(space));
        assert_eq!(f.s.guest_received(), expected);
        let release = Stroke {
            state: RELEASED,
            serial: 703,
            time: 1_500,
            ..space
        };
        f.s.host([f.peek(release)]).await;
        assert!(!f.held(SPACE));
        // No peer sees the peek a later confirmation could recover.
        let keyboard = HostId(f.keyboard.host);
        assert_eq!(f.s.ctx().key_generations.peek(keyboard, SPACE), None);

        // Backspace held over a preedit the input method empties.
        let backspace = Stroke {
            key: BACKSPACE,
            state: PRESSED,
            serial: 800,
            time: 1_600,
        };
        f.s.host([f.peek(backspace), f.preedit("가", ""), f.preedit("", "")])
            .await;
        let expected = [preedit("가"), DONE, no_preedit(), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([f.confirm(), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        let mut expected = f.recovered(backspace);
        expected.extend(f.recovered(backspace));
        assert_eq!(f.s.guest_received(), expected);
        let keysym_release = Stroke {
            state: RELEASED,
            serial: 801,
            time: 1_601,
            ..backspace
        };
        f.s.host([
            f.keysym(KEY_BackSpace, backspace),
            f.keysym(KEY_BackSpace, keysym_release),
            f.key(backspace),
        ])
        .await;
        assert_eq!(f.s.guest_received(), []);
        assert_eq!(f.s.host_received(), [f.ack(800, false)]);

        // Its release reaches the proxy on both channels.
        let release = Stroke {
            state: RELEASED,
            serial: 801,
            time: 1_700,
            ..backspace
        };
        f.s.host([f.peek(release), f.key(release), f.confirm()])
            .await;
        assert_eq!(f.s.guest_received(), []);
        assert_eq!(f.s.host_received(), [f.ack(801, false)]);

        let backspace = Stroke {
            serial: 810,
            time: 1_800,
            ..backspace
        };
        f.s.host([f.peek(backspace), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(backspace));
    }

    /// The keys a `wl_keyboard.enter` reports held are numbered in the
    /// host's order, each at its first listing, and a peek of such a key
    /// takes its number. So when the host lists S, then A, then S again,
    /// and peeks S, then A, an empty confirmation stands for A, the key
    /// numbered last and peeked last: the order follows the host, not the
    /// keycodes. The guest holds A since the enter, so it gets no key
    /// pair, only the `done` that closes the transaction. Numbered by
    /// keycode, A would come first, the latest peek would not be the
    /// newest key, and the confirmation would stand for nothing.
    #[tokio::test]
    async fn keys_held_at_an_enter_are_numbered_in_the_hosts_order() {
        const KEY_S: u32 = 31;
        const KEY_A: u32 = 30;
        let mut f = Field::offered(Host {
            text_input_extension: EXTENSION,
            keyboard_extension_with_keymap: true,
        })
        .await;
        f.keyboard_leave().await;
        let keys: Vec<u8> = [KEY_S, KEY_A, KEY_S]
            .iter()
            .flat_map(|key| key.to_ne_bytes())
            .collect();
        let enter = wl_keyboard::evt::enter(f.keyboard.host, 3, f.surface.host, &keys);
        f.s.host([enter.unwrap()]).await;
        let (keyboard, surface) = (f.keyboard.id, f.surface.id);
        let modifiers = wl_keyboard::Event::Modifiers {
            serial: 3,
            mods_depressed: 0,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        let mut expected: Vec<(u32, AnyEvent)> = vec![
            (
                keyboard,
                wl_keyboard::Event::Enter {
                    serial: 3,
                    surface,
                    keys,
                }
                .into(),
            ),
            (keyboard, modifiers.into()),
        ];
        expected.extend(f.events([V3::Enter { surface }]));
        assert_eq!(f.s.guest_received(), expected);
        assert_eq!(f.s.host_received(), []);
        f.start_typing().await;

        let s = Stroke {
            key: KEY_S,
            state: PRESSED,
            serial: 800,
            time: 2_000,
        };
        let a = Stroke {
            key: KEY_A,
            serial: 801,
            time: 2_001,
            ..s
        };
        f.s.host([f.peek(s), f.peek(a), f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.events([DONE]));
    }

    /// A text input destroyed while its deactivation drains sends nothing
    /// more: its host's late events reach no one, and the proxy's drain
    /// callback ends without reaching the guest, its host ID free for
    /// another object only after the host's `delete_id`.
    #[tokio::test]
    async fn a_text_input_destroyed_while_draining_ends_its_barrier_unseen() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));
        f.s.guest([v3::destroy(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.ext(Ext::Destroy {})]);
        let deleted = wl_display::Event::DeleteId { id: f.ti };
        assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        f.s.host([f.preedit("stale", ""), f.commit("stale")]).await;
        assert_eq!(f.s.guest_received(), []);

        f.drain(callback).await;
        // No peer sees whether the proxy still waits on its own callback.
        assert_eq!(f.s.ctx().internal_callbacks.get(HostId(callback)), None);
        let ti = f.s.new_id();
        f.create_text_input(ti).await;
        assert!(![f.host, f.host_ext].contains(&callback));
        f.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, callback)])
            .await;
        assert_eq!(f.s.guest_received(), []);
        let ti = f.s.new_id();
        f.create_text_input(ti).await;
        assert_eq!(f.host, callback);
    }

    /// A focus change drains the old generation: the input method's
    /// composition, commit and confirmation sent while it drains reach no
    /// one, while the guest's enable on the new surface waits for the
    /// drain and then composes.
    #[tokio::test]
    async fn a_focus_change_drops_the_old_composition_and_held_key() {
        let mut f = Field::typing().await;
        let space = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: 700,
            time: 1_000,
        };
        f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
            .await;
        let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(space));

        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);
        f.keyboard_enter_new_surface().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(
            f.s.host_received(),
            [
                f.surrounding_text("", 0, 0),
                f.no_cursor_rectangle(),
                f.commit_state(2)
            ]
        );
        f.answered(2);

        f.s.host([
            f.preedit("오래된 조합", ""),
            f.commit("오래된 확정"),
            f.confirm(),
        ])
        .await;
        assert_eq!(f.s.guest_received(), []);

        f.drain(callback).await;
        // No peer sees whether the proxy still waits on its own callback.
        assert_eq!(f.s.ctx().internal_callbacks.get(HostId(callback)), None);
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, 0), false));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);
        f.s.host([f.preedit("새 조합", "")]).await;
        let expected = [preedit("새 조합"), V3::Done { serial: 2 }];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// A text input recreated under the ID of one whose deactivation
    /// drains is activated at once, and the old drain neither reconciles
    /// nor reaches it, nor moves it off its own host object.
    #[tokio::test]
    async fn a_text_input_recreated_under_its_id_ignores_the_old_barrier() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));
        f.s.guest([v3::destroy(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.ext(Ext::Destroy {})]);
        let deleted = wl_display::Event::DeleteId { id: f.ti };
        assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        let old_host = f.host;
        f.create_text_input(f.ti).await;
        assert_ne!(f.host, old_host);
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        f.answered(1);

        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
        assert_eq!(f.s.guest_received(), []);
        f.s.host([f.preedit("새 객체", "")]).await;
        let expected = [preedit("새 객체"), DONE];
        assert_eq!(f.s.guest_received(), f.events(expected));
        // The replacement keeps its own host text input, which its disable
        // deactivates.
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        assert_eq!(host, f.disable_requests(barrier(&host), 2));
    }

    /// An enable or a disable the guest has not committed when the focus
    /// leaves dies with that focus: its commit after the next enter
    /// activates nothing.
    #[tokio::test]
    async fn an_uncommitted_enable_or_disable_dies_with_its_focus() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);
        f.keyboard_enter_new_surface().await;
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
        // No peer sees whether the proxy still waits on its own callback.
        assert_eq!(f.s.ctx().internal_callbacks.get(HostId(callback)), None);
        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);

        f.s.guest([v3::enable(f.ti)]).await;
        f.keyboard_leave().await;
        assert_eq!(f.s.host_received(), []);
        f.keyboard_enter_new_surface().await;
        // No peer sees that the enter dropped the pending enable before
        // the commit below shows it.
        assert_eq!(f.state().pending_for_test().enabled, None);
        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.commit_state(3)]);
    }

    /// A release delayed past a wrapped serial's next press ends only its
    /// own generation: the new press stays held and owned, repeats, and
    /// gets its own release, and every key gets one `ack_key`. Then a key
    /// is held as the focus leaves, which drains the field: a confirmation
    /// recovers nothing.
    #[tokio::test]
    async fn a_delayed_release_ends_only_its_generation() {
        let mut f = Field::typing().await;
        let old_press = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: u32::MAX - 2,
            time: 100,
        };
        let old_release = Stroke {
            state: RELEASED,
            serial: u32::MAX,
            time: 110,
            ..old_press
        };
        let new_press = Stroke {
            serial: 1,
            time: 200,
            ..old_press
        };
        f.s.host([f.peek(old_press), f.key(old_press)]).await;
        assert_eq!(f.s.guest_received(), [f.guest_key(old_press)]);
        assert_eq!(f.s.host_received(), [f.ack(old_press.serial, true)]);
        f.s.host([f.peek(old_release), f.peek(new_press), f.key(new_press)])
            .await;
        assert_eq!(f.s.guest_received(), [f.guest_key(new_press)]);
        assert_eq!(f.s.host_received(), [f.ack(new_press.serial, true)]);

        f.s.host([f.key(old_release)]).await;
        assert_eq!(f.s.guest_received(), [f.guest_key(old_release)]);
        assert_eq!(f.s.host_received(), [f.ack(old_release.serial, true)]);
        assert!(f.held(SPACE));
        assert_eq!(f.owner(SPACE), Some(GuestKeyOwner::Physical));

        let repeat = Stroke {
            state: REPEATED,
            serial: 2,
            time: 210,
            ..old_press
        };
        f.s.host([f.key(repeat)]).await;
        assert_eq!(f.s.guest_received(), [f.guest_key(repeat)]);
        assert_eq!(f.s.host_received(), [f.ack(repeat.serial, true)]);
        let new_release = Stroke {
            state: RELEASED,
            serial: 3,
            time: 220,
            ..old_press
        };
        f.s.host([f.peek(new_release), f.key(new_release)]).await;
        assert_eq!(f.s.guest_received(), [f.guest_key(new_release)]);
        assert_eq!(f.s.host_received(), [f.ack(new_release.serial, true)]);
        assert!(!f.held(SPACE));
        assert_eq!(f.owner(SPACE), None);

        let held = Stroke {
            serial: 4,
            time: 300,
            ..old_press
        };
        f.s.host([f.peek(held)]).await;
        f.keyboard_leave().await;
        let host = f.s.host_received();
        assert_eq!(host, [f.deactivate(), sync(barrier(&host))]);
        f.s.host([f.confirm()]).await;
        assert_eq!(f.s.guest_received(), []);
    }

    /// Each keyboard of a seat gets one `enter` and one `leave` for each
    /// focus it is given, whatever order the host's events take, and the
    /// seat's focus, which its text input follows, is the focus of the
    /// keyboard that entered last.
    #[tokio::test]
    async fn each_keyboard_focus_is_entered_and_left_once() {
        // Keyboard `a` has entered the surface with serial 1.
        let mut f = Field::disabled().await;
        let (a, surface) = (f.keyboard, f.surface);
        let id = f.s.new_id();
        let host = f.s.create(wl_seat::req::get_keyboard(f.seat.id, id)).await;
        let b = Object { id, host };
        let replacement = f.s.surface().await;
        let enter = |keyboard: Object, serial, surface: Object| {
            wl_keyboard::evt::enter(keyboard.host, serial, surface.host, &[]).unwrap()
        };
        let leave = |keyboard: Object, serial, surface: Object| {
            wl_keyboard::evt::leave(keyboard.host, serial, surface.host)
        };
        let entered = |keyboard: Object, serial, surface: Object| {
            let enter = wl_keyboard::Event::Enter {
                serial,
                surface: surface.id,
                keys: Vec::new(),
            };
            let modifiers = wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed: 0,
                mods_latched: 0,
                mods_locked: 0,
                group: 0,
            };
            [(keyboard.id, enter.into()), (keyboard.id, modifiers.into())]
        };
        let left = |keyboard: Object, serial, surface: Object| {
            let leave = wl_keyboard::Event::Leave {
                serial,
                surface: surface.id,
            };
            (keyboard.id, leave.into())
        };

        // A second keyboard needs its own enter; a duplicate is dropped.
        f.s.host([enter(b, 2, surface), enter(b, 3, surface)]).await;
        assert_eq!(f.s.guest_received(), entered(b, 2, surface));
        // The seat keeps its focus through `b`.
        f.s.host([leave(a, 4, surface), leave(a, 5, surface)]).await;
        assert_eq!(f.s.guest_received(), [left(a, 4, surface)]);
        f.s.host([leave(b, 6, surface)]).await;
        let mut expected = vec![left(b, 6, surface)];
        expected.extend(f.events([V3::Leave {
            surface: surface.id,
        }]));
        assert_eq!(f.s.guest_received(), expected);

        // `b` takes the seat's focus elsewhere before `a` leaves: that
        // leave still balances `a`'s enter, once, and moves no focus.
        f.s.host([enter(a, 7, surface)]).await;
        let mut expected = Vec::from(entered(a, 7, surface));
        expected.extend(f.events([V3::Enter {
            surface: surface.id,
        }]));
        assert_eq!(f.s.guest_received(), expected);
        f.s.host([enter(b, 8, replacement)]).await;
        let mut expected = Vec::from(entered(b, 8, replacement));
        expected.extend(f.events([
            V3::Leave {
                surface: surface.id,
            },
            V3::Enter {
                surface: replacement.id,
            },
        ]));
        assert_eq!(f.s.guest_received(), expected);
        f.s.host([leave(a, 9, surface), leave(a, 10, surface)])
            .await;
        assert_eq!(f.s.guest_received(), [left(a, 9, surface)]);
    }

    /// The fixture's guest text input, its host text input and extension.
    pub(in crate::handler) const TI: u32 = 20;
    pub(in crate::handler) const HOST: u32 = 10;
    pub(in crate::handler) const HOST_EXT: u32 = 30;
    /// The fixture's guest seat and focused surface, and their host objects.
    pub(in crate::handler) const SEAT: u32 = 5;
    pub(in crate::handler) const SURFACE: u32 = 900;
    pub(in crate::handler) const HOST_SEAT: u32 = 6;
    pub(in crate::handler) const HOST_SURFACE: u32 = 901;
    /// The guest keyboard [`Ime::with_keyboard`] adds, its host keyboard
    /// and that keyboard's extended keyboard.
    pub(in crate::handler) const KEYBOARD: u32 = 40;
    pub(in crate::handler) const HOST_KEYBOARD: HostId = HostId(41);
    pub(in crate::handler) const HOST_EXTENDED_KEYBOARD: u32 = 42;
    pub(in crate::handler) const BACKSPACE: u32 = crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
    pub(in crate::handler) const PRESSED: u32 = wl_keyboard::KEY_STATE_PRESSED;
    pub(in crate::handler) const RELEASED: u32 = wl_keyboard::KEY_STATE_RELEASED;
    pub(in crate::handler) const REPEATED: u32 = wl_keyboard::KEY_STATE_REPEATED;

    /// Handler tests of key and IME state that the peers of a [`Session`]
    /// cannot set up exactly: the guest text input [`TI`] of seat [`SEAT`]
    /// is enabled and its host active on surface [`SURFACE`].
    pub(in crate::handler) struct Ime {
        pub(in crate::handler) ctx: Context,
    }

    impl Ime {
        pub(in crate::handler) fn new() -> Self {
            Self::with_extension_version(Some(zcr_extended_text_input_v1::VERSION))
        }

        /// The fixture whose extension has `version` in the object table,
        /// or none.
        pub(in crate::handler) fn with_extension_version(version: Option<u32>) -> Self {
            let mut ctx = Context::new_for_test(false, false, vec![]);
            let table = &mut ctx.shadow_table;
            table.insert_paired_for_test(SEAT, HOST_SEAT, "wl_seat");
            table.insert_paired_for_test(SURFACE, HOST_SURFACE, "wl_surface");
            ctx.shadow_table.insert_bridged_for_test(
                TI,
                HOST,
                "zwp_text_input_v3",
                "zwp_text_input_v1",
            );
            if let Some(version) = version {
                let interface = "zcr_extended_text_input_v1";
                ctx.shadow_table
                    .insert_host_only_for_test(HOST_EXT, interface, version);
            }
            let host = HostTi {
                v1: HOST,
                ext: Some(HostExt {
                    id: HOST_EXT,
                    version: ctx.shadow_table.host_object_version_or_unknown(HOST_EXT),
                    manager: 0,
                }),
            };
            let ti = TiId(TI);
            ctx.ime
                .insert_for_test(ti, SeatId(SEAT), host, Some(SurfaceId(SURFACE)));
            let mut ime = Self { ctx };
            ime.committed().enabled = true;
            ime.settle();
            ime
        }

        /// Attach the host text input with the content type the field
        /// calls for, as the reconcile of the guest commit that set its
        /// state would. A host event reconciles every text input, and
        /// would send it otherwise.
        pub(in crate::handler) fn settle(&mut self) {
            let env = CtxEnv::new(&self.ctx.shadow_table, &self.ctx.keyboard_focus);
            let t = self.ctx.ime.get_mut(TiId(TI)).expect("the text input");
            t.attach_for_test(&env);
        }

        /// The fixture with guest keyboard [`KEYBOARD`] on the seat.
        pub(in crate::handler) fn with_keyboard() -> Self {
            let mut ime = Self::new();
            ime.keyboard(KEYBOARD, HOST_KEYBOARD);
            ime
        }

        /// Add guest keyboard `guest`, whose host keyboard is `host`, to the
        /// seat.
        pub(in crate::handler) fn keyboard(&mut self, guest: u32, host: HostId) {
            self.ctx
                .shadow_table
                .insert_paired_for_test(guest, host.0, "wl_keyboard");
            self.ctx.keyboard_to_seat.insert(guest, SEAT);
        }

        /// Give [`HOST_KEYBOARD`] its extended keyboard, which sends
        /// `peek_key`.
        pub(in crate::handler) fn with_extended_keyboard() -> Self {
            let mut ime = Self::with_keyboard();
            let extended = HostId(HOST_EXTENDED_KEYBOARD);
            ime.ctx
                .keyboard_to_extended_keyboard
                .insert(HOST_KEYBOARD, extended);
            ime.ctx
                .extended_keyboard_to_keyboard
                .insert(extended, HOST_KEYBOARD);
            ime
        }

        /// Give the seat's keyboard focus on the surface to `keyboard`.
        pub(in crate::handler) fn focus(&mut self, keyboard: HostId) {
            self.ctx
                .keyboard_focus
                .set_for_test(keyboard, SEAT, SURFACE, HOST_SURFACE);
        }

        pub(in crate::handler) fn state(&mut self) -> &mut crate::ime::TextInput {
            self.ctx.ime.get_mut(TiId(TI)).expect("the text input")
        }

        /// The committed state of the field, which is focused.
        pub(in crate::handler) fn committed(&mut self) -> &mut crate::ime::V3Committed {
            let state = self.state();
            state.committed_mut_for_test().expect("a focused field")
        }

        pub(in crate::handler) fn preedit(&mut self, text: &str, commit: &str) {
            self.ctx.last_sender_id = HOST;
            let (text, commit) = (IndexedString::from(text), commit.to_string());
            TextInputV1Handler.on_preedit_string(&mut self.ctx, 0, &text, &commit);
        }

        pub(in crate::handler) fn commit(&mut self, text: &str) {
            self.ctx.last_sender_id = HOST;
            TextInputV1Handler.on_commit_string(&mut self.ctx, 0, &text.to_string());
        }

        pub(in crate::handler) fn preedit_region(&mut self, index: i32, length: u32) {
            self.ctx.last_sender_id = HOST_EXT;
            ExtendedTextInputV1Handler.on_set_preedit_region(&mut self.ctx, index, length);
        }

        /// The host input method's empty confirmation, then a host event
        /// that shows it to be an autorepeat's, such as the `done` of the
        /// sync the proxy sends with a recovery that waits
        /// ([`crate::handler::ime_keys::PendingRecovery`]).
        pub(in crate::handler) fn confirm(&mut self) {
            self.ctx.last_sender_id = HOST_EXT;
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut self.ctx, 0);
            crate::handler::ime_keys::recover_pending(&mut self.ctx);
        }

        /// The host's `keysym` of key `serial` at `time`, which Exo sends
        /// in each other's arguments.
        pub(in crate::handler) fn keysym(&mut self, sym: u32, state: u32, serial: u32, time: u32) {
            self.ctx.last_sender_id = HOST;
            TextInputV1Handler.on_keysym(&mut self.ctx, time, serial, sym, state, 0);
        }

        pub(in crate::handler) fn peek(&mut self, key: u32, state: u32, serial: u32, time: u32) {
            self.ctx.last_sender_id = HOST_EXTENDED_KEYBOARD;
            KeyboardHandler::new().on_peek_key(&mut self.ctx, serial, time, key, state);
        }

        /// The host keyboard's `key`; returns whether it reaches the guest.
        pub(in crate::handler) fn key(
            &mut self,
            key: u32,
            state: u32,
            serial: u32,
            time: u32,
        ) -> Action {
            self.ctx.last_sender_id = HOST_KEYBOARD.0;
            KeyboardHandler::new().on_key(&mut self.ctx, serial, time, key, state)
        }

        /// Hold repeatable `key` on `keyboard`, whose press `peek_key`
        /// reported with `serial` at `time`.
        pub(in crate::handler) fn hold_peeked(
            &mut self,
            keyboard: HostId,
            key: u32,
            serial: u32,
            time: u32,
        ) {
            self.ctx
                .key_generations
                .observe_peek_press(keyboard, key, serial, time, true);
            self.ctx
                .keyboard_repeatable_keys
                .entry(keyboard)
                .or_default()
                .insert(key);
        }

        /// Hold Backspace on [`HOST_KEYBOARD`], with its IME repeat leased
        /// to `text_input`.
        pub(in crate::handler) fn lease_backspace_repeat(&mut self, text_input: u32) {
            let generations = &mut self.ctx.key_generations;
            generations.observe_physical_state(HOST_KEYBOARD, BACKSPACE, 1);
            assert!(generations.arm_ime_repeat(HOST_KEYBOARD, BACKSPACE, text_input));
        }

        /// Hold `key` on [`HOST_KEYBOARD`], with its guest press owned by
        /// `owner`.
        pub(in crate::handler) fn hold_owned(&mut self, key: u32, owner: GuestKeyOwner) {
            let generations = &mut self.ctx.key_generations;
            generations.observe_physical_state(HOST_KEYBOARD, key, 1);
            assert!(self.ctx.claim_guest_key(HOST_KEYBOARD, key, owner));
        }

        pub(in crate::handler) fn repeat_active(&self) -> bool {
            backspace_repeat_active_for_keyboard(&self.ctx, HOST_KEYBOARD)
        }

        pub(in crate::handler) fn owner(&self, key: u32) -> Option<GuestKeyOwner> {
            self.ctx.guest_key_owner(HOST_KEYBOARD, key)
        }

        /// The events queued for the guest since the last call.
        pub(in crate::handler) fn events(&mut self) -> Vec<(u32, AnyEvent)> {
            let queue = std::mem::take(&mut self.ctx.host_to_client_queue);
            let table = &self.ctx.shadow_table;
            queue
                .iter()
                .map(|(message, _)| {
                    crate::test_support::decode(message, &mut Default::default(), |msg| {
                        let interface = table.get_interface(msg.sender_id).expect("a sender");
                        AnyEvent::from_wire(interface, msg)
                    })
                    .expect("a queued event decodes")
                })
                .collect()
        }

        /// The requests queued for the host since the last call.
        pub(in crate::handler) fn requests(&mut self) -> Vec<(u32, AnyRequest)> {
            let queue = std::mem::take(&mut self.ctx.client_to_host_queue);
            let table = &self.ctx.shadow_table;
            queue
                .iter()
                .map(|(message, _)| {
                    crate::test_support::decode(message, &mut Default::default(), |msg| {
                        let interface = match msg.sender_id {
                            HOST_EXT => "zcr_extended_text_input_v1",
                            host => table.get_host_interface(host).expect("a sender"),
                        };
                        AnyRequest::from_wire(interface, msg)
                    })
                    .expect("a queued request decodes")
                })
                .collect()
        }
    }

    pub(in crate::handler) fn key(
        keyboard: u32,
        key: u32,
        state: u32,
        serial: u32,
        time: u32,
    ) -> (u32, AnyEvent) {
        let event = wl_keyboard::Event::Key {
            serial,
            time,
            key,
            state,
        };
        (keyboard, event.into())
    }

    /// The fixture's `done`, before any guest commit.
    pub(in crate::handler) fn uncommitted_done() -> (u32, AnyEvent) {
        (TI, V3::Done { serial: 0 }.into())
    }

    /// A commit whose surrounding text does not fit a message reaches the
    /// host with nothing: it consumes its pending state and changes
    /// nothing else but the commit count.
    #[test]
    fn oversized_surrounding_commit_does_not_partially_advance_state() {
        let mut ime = Ime::new();
        let oversized = "x".repeat(4_084);
        let end = oversized.len() as i32;
        ime.ctx.last_sender_id = TI;
        let oversized = IndexedString::from(oversized.as_str());
        TextInputV3Handler.on_set_surrounding_text(&mut ime.ctx, &oversized, end, end);
        TextInputV3Handler.on_set_content_type(&mut ime.ctx, 0, 13);
        let mut expected = ime.state().clone();
        TextInputV3Handler.on_commit(&mut ime.ctx);

        assert!(ime.ctx.client_to_host_queue.is_empty());
        expected.set_commits_for_test(1);
        *expected.pending_mut().expect("a focused field") = crate::ime::V3Pending::default();
        assert_eq!(ime.state(), &expected);
        // The field stays enabled, so the commit gets its `done`.
        assert_eq!(ime.events(), [(TI, V3::Done { serial: 1 }.into())]);
    }

    /// The commit after one whose surrounding text did not fit does not
    /// retry that commit's pending state: it reaches the host with only
    /// what the guest set since.
    #[test]
    fn the_commit_after_an_oversized_one_does_not_retry_it() {
        let mut ime = Ime::new();
        ime.ctx.last_sender_id = TI;
        // The fixture's host has no content type yet, and the host's
        // content type says whether the field reports its text: this
        // commit settles both.
        let text = IndexedString::from("a");
        TextInputV3Handler.on_set_surrounding_text(&mut ime.ctx, &text, 1, 1);
        TextInputV3Handler.on_commit(&mut ime.ctx);
        ime.requests();
        ime.events();

        let oversized = IndexedString::from("x".repeat(4_084).as_str());
        TextInputV3Handler.on_set_surrounding_text(&mut ime.ctx, &oversized, 4_084, 4_084);
        let password = zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD;
        TextInputV3Handler.on_set_content_type(&mut ime.ctx, 0, password);
        TextInputV3Handler.on_commit(&mut ime.ctx);
        assert_eq!(ime.requests(), []);
        assert_eq!(ime.events(), [(TI, V3::Done { serial: 2 }.into())]);

        // Neither the text nor the content type is sent again.
        TextInputV3Handler.on_commit(&mut ime.ctx);
        let commit_state =
            |serial| -> (u32, AnyRequest) { (HOST, V1::CommitState { serial }.into()) };
        assert_eq!(ime.requests(), [commit_state(3)]);
        assert_eq!(ime.events(), [(TI, V3::Done { serial: 3 }.into())]);

        let text = IndexedString::from("ab");
        TextInputV3Handler.on_set_surrounding_text(&mut ime.ctx, &text, 2, 2);
        TextInputV3Handler.on_commit(&mut ime.ctx);
        let surrounding = V1::SetSurroundingText {
            text: "ab".into(),
            cursor: 2,
            anchor: 2,
        };
        assert_eq!(
            ime.requests(),
            [(HOST, surrounding.into()), commit_state(4)]
        );
        assert_eq!(ime.events(), [(TI, V3::Done { serial: 4 }.into())]);
    }

    /// A shown preedit the answer to a commit cannot repeat leaves the
    /// `done` to clear it, so the proxy forgets it too, and the guest and
    /// the proxy agree that no composition shows. The proxy shows only
    /// preedits it has sent, so no host event reaches this.
    #[test]
    fn an_answer_that_cannot_repeat_the_preedit_forgets_it() {
        let mut ime = Ime::new();
        ime.state().show_for_test(&"x".repeat(4_084));
        ime.ctx.last_sender_id = TI;
        TextInputV3Handler.on_commit(&mut ime.ctx);
        assert_eq!(ime.events(), [(TI, V3::Done { serial: 1 }.into())]);
        assert_eq!(ime.state().shown_for_test(), None);
    }

    /// Host text too long for a message is dropped whole: the guest gets
    /// nothing, and the text input and its repeat lease stay as they were,
    /// except that the event takes what the host recorded for it: a
    /// preedit, preedit region or confirmation the preedit cursor and
    /// styling, a commit the cursor position. Otherwise the next one would
    /// apply them. Only host
    /// text the proxy lengthened by replacing invalid UTF-8 fits the host's
    /// message but not the guest's (`host_ime_events_become_v3_transactions`
    /// sends such a commit); this test reaches every such drop directly.
    #[test]
    fn oversized_host_text_does_not_partially_advance_state() {
        let oversized = "x".repeat(4_084);
        type Event = fn(&mut Ime, &str);
        let cases: [(&str, Event); 4] = [
            ("preedit", |ime, text| ime.preedit(text, "")),
            ("commit", |ime, text| ime.commit(text)),
            ("preedit region", |ime, _| ime.preedit_region(-4_084, 4_084)),
            ("confirm", |ime, _| ime.confirm()),
        ];
        for (case, event) in cases {
            let mut ime = Ime::with_keyboard();
            ime.committed().surrounding_text = Some((oversized.clone(), 4_084, 4_084));
            ime.settle();
            let state = ime.state();
            state.show_for_test(&oversized);
            state.set_assembly_for_test(crate::ime::Assembly {
                cursor: Some(1),
                selection: Some((0, 1)),
            });
            ime.lease_backspace_repeat(TI);
            let mut expected = ime.state().clone();
            if case != "commit" {
                expected.set_assembly_for_test(crate::ime::Assembly::default());
            }

            event(&mut ime, &oversized);
            assert_eq!(ime.events(), [], "{case}");
            assert_eq!(ime.state(), &expected, "{case}");
            assert!(ime.repeat_active(), "{case}");
        }
    }

    #[test]
    fn child_objects_inherit_their_manager_versions() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_text_input_manager_v1_id = Some(2);
        ctx.host_text_input_extension_v1_id = Some(3);
        // Both text-input protocols are v1, so artificial manager versions
        // show where each child's version comes from.
        ctx.shadow_table
            .insert_host_only_for_test(2, "zwp_text_input_manager_v1", 4);
        ctx.shadow_table
            .insert_host_only_for_test(3, "zcr_text_input_extension_v1", 7);
        ctx.shadow_table
            .create_local(40, "zwp_text_input_manager_v3", 5);
        ctx.last_sender_id = 40;

        let mut handler = TextInputManagerV3Handler;
        assert_eq!(handler.on_get_text_input(&mut ctx, 20, 30), Action::Drop);
        let host = ctx.ime.get(TiId(20)).expect("the text input").host();
        let child_id = host.ext.expect("extended text input child").id;
        assert_eq!(
            ctx.shadow_table.host_object_version(child_id),
            Some(7),
            "version guards must use the manager's negotiated host version"
        );
        assert_eq!(ctx.shadow_table.host_object_version(host.v1), Some(4));
        assert_eq!(ctx.shadow_table.guest_object_version(20), Some(5));
    }

    /// A drain barrier completes only the generation it drains: the guest
    /// destroyed the text input and reused its ID for one with another
    /// host text input, enabled and not yet active, as it waits for a
    /// drain of its own, which the old barrier must not end.
    #[test]
    fn stale_activation_callback_cannot_reconcile_reused_guest_id() {
        let mut ime = Ime::new();
        ime.focus(HOST_KEYBOARD);
        let ti = TiId(TI);
        forget_text_input(&mut ime.ctx, TI, false);
        // The deactivate, its drain barrier, then the extension's destroy.
        let queue = std::mem::take(&mut ime.ctx.client_to_host_queue);
        let [(_, wl_display::Request::Sync { callback })] =
            decoded(&queue[1..2], wl_display::Request::from_wire)[..]
        else {
            panic!("the destroy drains behind a barrier");
        };
        let replacement = HostTi { v1: 11, ext: None };
        let create = Input::Create {
            ti,
            seat: SeatId(SEAT),
            host: replacement,
            manager: 2,
        };
        // It activates, deactivates, and is enabled again while it drains.
        for input in [
            create,
            Input::Enable(ti),
            Input::Commit(ti),
            Input::Disable(ti),
            Input::Commit(ti),
            Input::Enable(ti),
            Input::Commit(ti),
        ] {
            super::ime(&mut ime.ctx, input);
        }
        assert!(matches!(ime.state().link(), LinkState::Draining { .. }));
        ime.ctx.client_to_host_queue.clear();

        ime.ctx.last_sender_id = callback;
        CallbackHandler.on_done(&mut ime.ctx, 0);
        assert!(!ime.state().is_attached());
        assert!(ime.ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn guest_commit_serial_wraps_and_is_forwarded_unchanged() {
        let mut ime = Ime::new();
        let state = ime.state();
        state.set_commits_for_test(u32::MAX);
        state.set_link_for_test(LinkState::Detached);
        ime.ctx.last_sender_id = TI;
        TextInputV3Handler.on_commit(&mut ime.ctx);

        assert_eq!(ime.state().serial().get(), 0);
        let queue = std::mem::take(&mut ime.ctx.client_to_host_queue);
        let (activate, extension, commit_state) = (&queue[..1], &queue[1..3], &queue[3..]);
        let activate = decoded(activate, V1::from_wire);
        assert_eq!(
            activate,
            [(
                HOST,
                V1::Activate {
                    seat: HOST_SEAT,
                    surface: HOST_SURFACE
                }
            )]
        );
        let extension = decoded(extension, Ext::from_wire);
        assert!(extension.iter().all(|(sender, _)| *sender == HOST_EXT));
        let commit_state = decoded(commit_state, V1::from_wire);
        assert_eq!(commit_state, [(HOST, V1::CommitState { serial: 0 })]);
    }

    /// An extension whose version the object table lacks is taken to have
    /// every request, as the newest does.
    #[test]
    fn an_extension_without_a_version_takes_the_newest_requests() {
        use zcr_extended_text_input_v1::{
            INLINE_COMPOSITION_SUPPORT_SUPPORTED, INPUT_FLAGS_HAS_BEEN_PASSWORD,
            INPUT_MODE_NUMERIC, INPUT_TYPE_NUMBER, LEARNING_MODE_DISABLED,
            SURROUNDING_TEXT_SUPPORT_UNSUPPORTED,
        };
        let mut ime = Ime::with_extension_version(None);
        ime.ctx.last_sender_id = TI;
        let pin = zwp_text_input_v3::CONTENT_PURPOSE_PIN;
        TextInputV3Handler.on_set_content_type(&mut ime.ctx, 0, pin);
        TextInputV3Handler.on_commit(&mut ime.ctx);

        let support = Ext::SetSurroundingTextSupport {
            support: SURROUNDING_TEXT_SUPPORT_UNSUPPORTED,
        };
        let set_input_type = Ext::SetInputType {
            input_type: INPUT_TYPE_NUMBER,
            input_mode: INPUT_MODE_NUMERIC,
            input_flags: INPUT_FLAGS_HAS_BEEN_PASSWORD,
            learning_mode: LEARNING_MODE_DISABLED,
            inline_composition_support: INLINE_COMPOSITION_SUPPORT_SUPPORTED,
        };
        assert_eq!(
            ime.requests(),
            [
                (HOST_EXT, support.into()),
                (HOST_EXT, set_input_type.into()),
                (HOST, V1::CommitState { serial: 1 }.into()),
            ]
        );
    }

    #[test]
    fn text_input_creation_without_host_manager_does_not_create_broken_state() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = TextInputManagerV3Handler;

        assert_eq!(handler.on_get_text_input(&mut ctx, 20, 7), Action::Drop);
        assert!(ctx.ime.get(TiId(20)).is_none());
        assert!(ctx.client_to_host_queue.is_empty());
    }
}
