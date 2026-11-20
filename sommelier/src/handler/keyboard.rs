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

//! Keyboard event handling with ChromeOS accelerator interception.
//!
//! When `zcr_keyboard_extension_v1` is available from the host, this handler
//! enables the ack-key protocol so that sommelier can decide per-key whether
//! the host compositor (ChromeOS/Exo) should process the key as an accelerator.
//!
//! Keys listed in `SOMMELIER_ACCELERATORS` are acked as `NOT_HANDLED` (host
//! runs the accelerator, key is not forwarded to guest). All other keys are
//! acked as `HANDLED` (guest receives the key, host skips the accelerator).
//! A chord bound to a placement action is consumed here and placed by
//! [`crate::handler::window_placement::place_focused`].
//!
//! Each key's press and release on `wl_keyboard` and on
//! `zcr_extended_keyboard_v1.peek_key` move its generation in
//! [`crate::state::KeyGenerationRegistry`], which decides whether the guest
//! gets the event; `docs/KEYBOARD.md` describes the states.
//!
//! See `sommelier/docs/KEYBOARD_SHORTCUT_INHIBITION.md` for the full protocol flow.

use crate::handler::ime_keys::lease;
use crate::keymap::{self, Keymap};
use crate::protocols::keyboard_extension_unstable_v1::{
    zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
};
use crate::protocols::wayland::wl_keyboard::{self, KEY_STATE_PRESSED, KEY_STATE_RELEASED};
use crate::state::{
    ChordClass, Context, HostId, KeyAdmission, KeyVerdict, KeyboardFocus, KeyboardModifiers,
    LeaseInput, PeekInput, PhysicalKey,
};
use crate::warn_once::WarnOnce;
use crate::window_layout::NormalizedRect;
use crate::wire::Action;
use std::os::fd::BorrowedFd;
use xkbcommon::xkb;

pub(crate) use crate::state::EVDEV_KEY_BACKSPACE;

/// Whether the proxy warned that the host has no
/// `zcr_keyboard_extension_v1` to acknowledge keys through. Such a host
/// sends every key so, to every client, so one warning per process says
/// it all.
static NO_KEYBOARD_EXTENSION_WARNED: WarnOnce = WarnOnce::new();

/// The level to log a key at that the host's missing
/// `zcr_keyboard_extension_v1` leaves unacknowledged: a warning the first
/// time for `once` (the process's [`NO_KEYBOARD_EXTENSION_WARNED`], or a
/// test's own), which this call records, and debug after.
fn take_no_keyboard_extension_log_level(once: &WarnOnce) -> log::Level {
    once.level()
}

/// Who owns a key, decided from its chord (see `KeyboardHandler::classify_key`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum KeyClass {
    /// Listed in `SOMMELIER_ACCELERATORS`: the host runs the accelerator.
    HostAccelerator,
    /// Bound to a placement while placement is on: Sommelier owns the
    /// chord, and places the focused window at the rectangle.
    Placement(NormalizedRect),
    /// Ordinary guest input.
    Guest,
}

impl KeyClass {
    /// The class the key generations know: placement is the keyboard's.
    fn chord(self) -> ChordClass {
        match self {
            Self::HostAccelerator => ChordClass::HostAccelerator,
            Self::Placement(_) => ChordClass::Shortcut,
            Self::Guest => ChordClass::Guest,
        }
    }
}

/// Keyboard handler that compiles each host keyboard's keymap, tracks its
/// XKB state for keysym resolution (in [`Context`]), and sends `ack_key`
/// responses to the host via `zcr_extended_keyboard_v1`.
pub struct KeyboardHandler {
    /// The XKB context keymaps are compiled in. It is `!Send` and `!Sync`,
    /// like the keymaps and states compiled from it.
    context: xkb::Context,
}

impl KeyboardHandler {
    pub fn new() -> Self {
        Self {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        }
    }

    fn guest_seat_for_host_keyboard(ctx: &Context, host_keyboard_id: HostId) -> Option<u32> {
        ctx.keyboards.seat(host_keyboard_id)
    }

    fn initialize_host_keyboard_enter_state(
        ctx: &mut Context,
        host_keyboard_id: HostId,
        keys: &[u8],
    ) {
        let (chunks, remainder) = keys.as_chunks::<4>();
        // The host's order, each key at its first listing: each key's
        // generation is numbered in this order, and a peek of a key held
        // since the enter takes its number.
        let mut pressed_keys = Vec::with_capacity(chunks.len());
        for key in chunks.iter().map(|chunk| u32::from_ne_bytes(*chunk)) {
            if !pressed_keys.contains(&key) {
                pressed_keys.push(key);
            }
        }
        if !remainder.is_empty() {
            log::warn!(
                "wl_keyboard.enter keys array has {} trailing byte(s)",
                remainder.len()
            );
        }

        ctx.key_generations
            .install_enter_snapshot(host_keyboard_id, pressed_keys);
    }

    /// Decide who owns a key from its chord alone.
    ///
    /// Both key channels (`wl_keyboard.key` and `peek_key`) ask this one
    /// classification. `Context::placement_rect_for` binds a chord only
    /// while placement is on, and the validator rejects a chord that is
    /// both a host accelerator and a binding; if one slipped through, the
    /// host keeps its accelerator.
    fn classify_key(ctx: &Context, host_keyboard_id: HostId, key: u32) -> KeyClass {
        let keymap = ctx
            .keyboards
            .get(host_keyboard_id)
            .and_then(|keyboard| keyboard.keymap.as_ref());
        // A keyboard with no keymap yet forms no chord.
        let Some(chord) = keymap.and_then(|keymap| keymap.chord(key)) else {
            return KeyClass::Guest;
        };
        let class = if ctx.is_host_accelerator(&chord) {
            KeyClass::HostAccelerator
        } else if let Some(action) = ctx.placement_rect_for(&chord) {
            KeyClass::Placement(action)
        } else {
            KeyClass::Guest
        };
        log::trace!(
            "key class: keyboard={} key={} chord={:?} class={:?}",
            host_keyboard_id.0,
            key,
            chord,
            class
        );
        class
    }

    /// Ensure the `zcr_extended_keyboard_v1` object is bound for this keyboard.
    ///
    /// This is idempotent: if the extended keyboard is already bound for
    /// `host_keyboard_id`, this function is a no-op.
    ///
    /// This is called from `on_enter` (rather than `wl_seat.get_keyboard`) because
    /// the host keyboard ID (`ctx.last_sender_id` in a host→client event) is only
    /// known once we process a host event — `get_keyboard` is a client→host request
    /// and at that point we only have the guest keyboard ID. `on_enter` is always
    /// sent by the host before any `wl_keyboard.key` event, so this is safe:
    /// the extended keyboard will be bound before the first key event.
    ///
    /// # Ordering invariant
    /// `get_extended_keyboard` is sent via `client_to_host_queue` and will be
    /// flushed in the same proxy loop iteration as the forwarded `wl_keyboard.enter`
    /// event. Exo enables `SetNeedKeyboardKeyAcks(true)` upon processing
    /// `get_extended_keyboard`. Any key events queued by the compositor *before*
    /// this request is processed (e.g. a key already in flight) will not have
    /// an active TTL in `pending_key_acks_`; Exo will apply its default policy for
    /// those keys. This mirrors the behavior of the C sommelier reference.
    pub(crate) fn ensure_extended_keyboard_bound(ctx: &mut Context, host_keyboard_id: HostId) {
        if let Some(extension_host_id) = ctx.host_keyboard_extension_id {
            let bound = ctx
                .keyboards
                .get(host_keyboard_id)
                .map(|k| k.extended.is_some());
            if bound == Some(false) {
                let Some(extended_version) =
                    ctx.shadow_table.host_object_version(extension_host_id.0)
                else {
                    let _ = ctx.fail(format_args!(
                        "keyboard extension {} has no version",
                        extension_host_id.0
                    ));
                    return;
                };
                let host_extended_id = HostId(
                    ctx.shadow_table
                        .create_host_only(zcr_extended_keyboard_v1::NAME, extended_version),
                );
                if let Some(keyboard) = ctx.keyboards.get_mut(host_keyboard_id) {
                    keyboard.extended = Some(host_extended_id);
                }

                ctx.client_to_host_queue.push(
                    zcr_keyboard_extension_v1::req::get_extended_keyboard(
                        extension_host_id.0,
                        host_extended_id.0,
                        host_keyboard_id.0,
                    ),
                );
                log::debug!(
                    "Bound extended keyboard: host_extended_id={} for host_keyboard_id={}",
                    host_extended_id.0,
                    host_keyboard_id.0
                );
            }
        }
    }

    /// How a key of `verdict` is dispatched.
    fn action(verdict: KeyVerdict) -> Action {
        match verdict.forward() {
            true => Action::Forward,
            false => Action::Drop,
        }
    }

    /// Send zcr_extended_keyboard_v1.ack_key to the host.
    ///
    /// The manager global may disappear after a child has been created. The
    /// child has its own protocol lifetime, so route acknowledgements from the
    /// child mapping directly and do not require the manager binding to remain.
    /// If no child has been bound for this keyboard, no acknowledgement is
    /// sent: either the key event arrived before `on_enter`, which the next
    /// enter mends, or the host has no extension, which only the first such
    /// key in the process warns about.
    fn send_ack_key(ctx: &mut Context, host_keyboard_id: HostId, serial: u32, handled: bool) {
        let extended = ctx.keyboards.get(host_keyboard_id).and_then(|k| k.extended);
        let Some(host_extended_id) = extended else {
            if ctx.host_keyboard_extension_id.is_some() {
                log::debug!(
                    "ack_key: no extended keyboard bound yet for host_keyboard_id={}; \
                     key event arrived before on_enter? serial={}, handled={}",
                    host_keyboard_id.0,
                    serial,
                    handled
                );
            } else {
                log::log!(
                    take_no_keyboard_extension_log_level(&NO_KEYBOARD_EXTENSION_WARNED),
                    "ack_key: the host has no zcr_keyboard_extension_v1, so keys go \
                     unacknowledged (host_keyboard_id={}, serial={}, handled={}); \
                     later ones are logged at debug",
                    host_keyboard_id.0,
                    serial,
                    handled
                );
            }
            return;
        };
        let handled = if handled {
            zcr_extended_keyboard_v1::HANDLED_STATE_HANDLED
        } else {
            zcr_extended_keyboard_v1::HANDLED_STATE_NOT_HANDLED
        };
        ctx.client_to_host_queue
            .push(zcr_extended_keyboard_v1::req::ack_key(
                host_extended_id.0,
                serial,
                handled,
            ));
    }
}

impl Default for KeyboardHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl wl_keyboard::WlKeyboardHandler for KeyboardHandler {
    /// Replace the keyboard's keymap, its XKB state and its repeat table
    /// with the one the host sent, or with none if the proxy cannot use it,
    /// and forward the event either way.
    ///
    /// The keys held and their generations stay, including the suppression
    /// of a held accelerator's key, which its press set and its release
    /// takes. Wayland may send a keymap, usable or not, while keys are held,
    /// as Exo does when Ctrl+Space switches the layout; ending their
    /// generations would send their releases to the host instead of the
    /// guest that got their presses, and leave the guest with stuck keys,
    /// and ending a suppression would let the IME hand the accelerator's
    /// key to the guest.
    fn on_keymap(
        &mut self,
        ctx: &mut Context,
        format: u32,
        fd: BorrowedFd<'_>,
        size: u32,
    ) -> Action {
        let host_keyboard_id = HostId::from_event_sender(ctx);
        let keymap = keymap::compile(&self.context, format, fd, size);
        if let Some(keyboard) = ctx.keyboards.get_mut(host_keyboard_id) {
            // Exo sends only the keymap on a layout change, so the
            // modifiers held and locked before it still apply.
            keyboard.keymap = keymap.map(|keymap| Keymap::new(&keymap, keyboard.modifiers));
            if keyboard.keymap.is_some() {
                log::info!("XKB keymap loaded successfully");
            }
        }
        Action::Forward
    }

    fn on_enter(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        surface: u32, // Host ID
        keys: &[u8],
    ) -> Action {
        // on_enter is a host→client event: last_sender_id is the host keyboard ID.
        let host_keyboard_id = HostId::from_event_sender(ctx);
        let guest_keyboard_id = ctx
            .shadow_table
            .get_guest_id(host_keyboard_id.0)
            .unwrap_or(0);
        let guest_surface_id = ctx.shadow_table.get_guest_id(surface).unwrap_or(0);

        // A surface keeps its numeric mapping until the host acknowledges the
        // forwarded wl_surface.destroy with wl_display.delete_id. A queued
        // keyboard.enter for that host ID can therefore still be translated,
        // but it must not resurrect the destroyed surface as IME focus.
        if guest_surface_id != 0 && ctx.shadow_table.is_pending_destroy_host(surface) {
            log::debug!(
                "Ignoring wl_keyboard.enter for pending-destroy surface {}",
                guest_surface_id
            );
            return Action::Drop;
        }
        if guest_surface_id == 0 {
            // Forwarding an enter with an unmapped object argument would turn
            // a stale host event into a guest protocol error. It must not
            // mutate the current focus generation or allocate extension
            // objects for an event that cannot be delivered.
            log::debug!(
                "Ignoring wl_keyboard.enter for unknown host surface {}",
                surface
            );
            return Action::Drop;
        }

        log::info!(
            ">>> wl_keyboard.on_enter: host_kb={:?}, guest_kb={}, surface={}, guest_surface={}",
            host_keyboard_id,
            guest_keyboard_id,
            surface,
            guest_surface_id
        );

        let Some(guest_seat_id) = ctx.keyboards.seat(host_keyboard_id) else {
            log::warn!("  -> host keyboard {} has no record", host_keyboard_id.0);
            return Action::Drop;
        };
        let focus = KeyboardFocus {
            guest_seat: guest_seat_id,
            guest_surface: guest_surface_id,
            host_surface: surface,
        };
        if ctx.keyboard_focus.focus_for_keyboard(host_keyboard_id) == Some(focus) {
            // The C reference suppresses an enter when the resource already
            // owns this focus. Preserve physical pressed-key and XKB state as
            // well; rebuilding it from an empty/partial keys array would make
            // the later release unmatched.
            log::debug!(
                "  -> duplicate enter for keyboard {} surface {}, preserving key state",
                host_keyboard_id.0,
                guest_surface_id
            );
            // wl_keyboard.enter is a state notification, not a request that
            // must be echoed. ChromiumOS suppresses a duplicate enter for an
            // already-focused resource; forwarding it would make the guest
            // restart its text-input transaction.
            return Action::Drop;
        }

        // Lazily bind the extended keyboard only after the event is known to
        // be deliverable. A malformed route must not allocate host objects or
        // queue extension requests before being rejected.
        Self::ensure_extended_keyboard_bound(ctx, host_keyboard_id);

        let focus_update = ctx.keyboard_focus.enter(host_keyboard_id, focus);
        for retired_keyboard in &focus_update.retired_keyboards {
            ctx.key_generations.clear_keyboard(*retired_keyboard);
        }
        Self::initialize_host_keyboard_enter_state(ctx, host_keyboard_id, keys);
        // The protocol requires a modifiers event after every enter, but Exo
        // sends one before it, and only when the modifiers changed. Send
        // the guest the keyboard's modifiers after the enter.
        let modifiers = ctx
            .keyboards
            .get(host_keyboard_id)
            .map(|keyboard| keyboard.modifiers)
            .unwrap_or_default();
        let enter = match wl_keyboard::evt::enter(guest_keyboard_id, serial, guest_surface_id, keys)
        {
            Ok(enter) => enter,
            Err(error) => {
                return ctx.fail(format_args!("Unable to resend wl_keyboard.enter: {error}"));
            }
        };
        let modifiers = wl_keyboard::evt::modifiers(
            guest_keyboard_id,
            serial,
            modifiers.depressed,
            modifiers.latched,
            modifiers.locked,
            modifiers.group,
        );

        if focus_update.seat_changes.is_empty() {
            // Another wl_keyboard resource can enter the seat's already
            // focused surface. The guest resource still needs its own enter,
            // but the seat-level text-input focus must remain untouched.
            // Exact duplicates for the same resource were rejected above.
            log::debug!(
                "  -> additional keyboard entered seat {} surface {}, leaving IME focus intact",
                guest_seat_id,
                guest_surface_id
            );
        }
        // The IME core sends them after the commit of a preedit the focus
        // change ends (F1).
        crate::handler::text_input::apply_keyboard_focus_changes(
            ctx,
            &focus_update.seat_changes,
            vec![enter, modifiers],
        );
        Action::Drop
    }

    fn on_leave(&mut self, ctx: &mut Context, serial: u32, surface: u32) -> Action {
        // on_leave is a host→client event: last_sender_id is the host keyboard ID.
        let host_keyboard_id = HostId::from_event_sender(ctx);
        let guest_keyboard_id = ctx
            .shadow_table
            .get_guest_id(host_keyboard_id.0)
            .unwrap_or(0);
        let guest_surface_id = ctx.shadow_table.get_guest_id(surface).unwrap_or(0);
        let surface_can_be_forwarded =
            guest_surface_id != 0 && !ctx.shadow_table.is_pending_destroy_host(surface);

        log::info!(
            ">>> wl_keyboard.on_leave: host_kb={:?}, guest_kb={}, surface={}, guest_surface={}",
            host_keyboard_id,
            guest_keyboard_id,
            surface,
            guest_surface_id
        );

        let focus_update = ctx.keyboard_focus.leave(host_keyboard_id, surface);
        if !focus_update.accepted {
            log::debug!(
                "  -> ignoring stale leave for keyboard {} host surface {}",
                host_keyboard_id.0,
                surface
            );
            return Action::Drop;
        }
        for retired_keyboard in &focus_update.retired_keyboards {
            ctx.key_generations.clear_keyboard(*retired_keyboard);
        }
        // The IME core sends the leave after the commit of a preedit the
        // focus change ends (F1).
        let leave = surface_can_be_forwarded
            .then(|| wl_keyboard::evt::leave(guest_keyboard_id, serial, guest_surface_id));
        crate::handler::text_input::apply_keyboard_focus_changes(
            ctx,
            &focus_update.seat_changes,
            leave.into_iter().collect(),
        );
        Action::Drop
    }

    /// Handle key events: resolve keysym, check against SOMMELIER_ACCELERATORS,
    /// and send ack_key to the host.
    fn on_key(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        _time: u32,
        key: u32,
        state: u32,
    ) -> Action {
        // on_key is a host→client event: last_sender_id is the host keyboard ID.
        let host_keyboard_id = HostId::from_event_sender(ctx);
        let guest_keyboard_id = ctx
            .shadow_table
            .get_guest_id(host_keyboard_id.0)
            .unwrap_or(0);
        let guest_seat = Self::guest_seat_for_host_keyboard(ctx, host_keyboard_id);
        log::trace!(
            ">>> wl_keyboard.on_key: host_kb={:?}, guest_kb={}, serial={}, key={}, state={}",
            host_keyboard_id,
            guest_keyboard_id,
            serial,
            key,
            state
        );
        // Read before the physical transition: a press that starts a new
        // generation of Backspace holds no lease.
        let backspace_repeat_active = key == EVDEV_KEY_BACKSPACE
            && ctx
                .key_generations
                .backspace_lease(host_keyboard_id)
                .is_some();
        if let KeyAdmission::Settled(verdict) =
            ctx.key_generations
                .admit_key(host_keyboard_id, key, state, serial)
        {
            Self::send_ack_key(ctx, host_keyboard_id, serial, verdict.handled());
            return Self::action(verdict);
        }

        let (event, class) = match state {
            KEY_STATE_PRESSED => {
                if key != EVDEV_KEY_BACKSPACE {
                    let keyboard = host_keyboard_id;
                    lease(ctx, guest_seat, LeaseInput::OtherPress { keyboard });
                }
                let class = Self::classify_key(ctx, host_keyboard_id, key);
                (
                    PhysicalKey::Press {
                        chord: class.chord(),
                        backspace_leased: backspace_repeat_active,
                    },
                    class,
                )
            }
            KEY_STATE_RELEASED => (PhysicalKey::Release, KeyClass::Guest),
            // Exo sends no repeats, and `wl_keyboard` has none before
            // version 10, beyond the version 5 the proxy binds `wl_seat` at.
            other => {
                log::warn!("on_key: received unknown key state {}, ignoring", other);
                return Action::Drop;
            }
        };
        let verdict = ctx
            .key_generations
            .transition_physical(host_keyboard_id, key, event);
        if let (KeyVerdict::Shortcut, KeyClass::Placement(action)) = (verdict, class) {
            // The policy owns every matched chord, so the chord is
            // consumed whether or not the window can move now; a skip
            // must not leak the key to the guest.
            if let Err(reason) =
                crate::handler::window_placement::place_focused(ctx, host_keyboard_id, action)
            {
                log::log!(
                    reason.log_level(),
                    "window layout {:?} skipped for keyboard {}: {:?}",
                    action,
                    host_keyboard_id.0,
                    reason
                );
            }
        }
        if state == KEY_STATE_RELEASED && key == EVDEV_KEY_BACKSPACE {
            lease(ctx, guest_seat, LeaseInput::BackspaceRelease);
        }
        Self::send_ack_key(ctx, host_keyboard_id, serial, verdict.handled());
        let action = Self::action(verdict);

        // A key of a keyboard with no keymap yet may be anything.
        let modifier = ctx
            .keyboards
            .get(host_keyboard_id)
            .and_then(|keyboard| keyboard.keymap.as_ref())
            .is_some_and(|keymap| keymap.is_modifier(key));
        if action == Action::Forward && state != KEY_STATE_RELEASED && !modifier {
            if let Some(guest_seat) = guest_seat {
                crate::handler::text_input::guest_key(ctx, guest_seat);
            }
        }
        log::debug!("  -> action={:?}", action);
        action
    }

    /// Track modifier state so on_key can resolve the correct keysym.
    ///
    /// The modifiers are kept even before the keyboard has a keymap: Exo
    /// sends them only when they change, so none may follow the keymap.
    fn on_modifiers(
        &mut self,
        ctx: &mut Context,
        _serial: u32,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) -> Action {
        log::trace!(
            ">>> wl_keyboard.on_modifiers: serial={}, depressed={:#x}, latched={:#x}, locked={:#x}, group={}",
            _serial, mods_depressed, mods_latched, mods_locked, group
        );
        let host_keyboard_id = HostId::from_event_sender(ctx);
        let modifiers = KeyboardModifiers {
            depressed: mods_depressed,
            latched: mods_latched,
            locked: mods_locked,
            group,
        };
        if let Some(keyboard) = ctx.keyboards.get_mut(host_keyboard_id) {
            keyboard.modifiers = modifiers;
            if let Some(keymap) = &mut keyboard.keymap {
                keymap.set_modifiers(modifiers);
            }
        }
        Action::Forward
    }
}

/// Forget guest keyboard `guest_keyboard_id` and its host keyboard
/// `host_keyboard_id`, which ended (see `crate::state::lifecycle`): end its
/// focus, destroy its extended keyboard, and drop its record and its key
/// state.
pub(crate) fn forget_keyboard(ctx: &mut Context, guest_keyboard_id: u32, host_keyboard_id: HostId) {
    crate::handler::ime_keys::forget_recovery_of_keyboard(ctx, guest_keyboard_id);
    let keyboard = ctx.keyboards.remove(host_keyboard_id);
    let focus_update = ctx.keyboard_focus.release(host_keyboard_id);
    // A re-created keyboard starts from a clean slate: an accelerator's key
    // suppressed under the old one would have its release swallowed on the
    // new one, and the old modifiers would classify its keys before its
    // first wl_keyboard.modifiers.
    ctx.key_generations.clear_keyboard(host_keyboard_id);
    if let Some(host_extended_id) = keyboard.and_then(|keyboard| keyboard.extended) {
        ctx.client_to_host_queue
            .push(zcr_extended_keyboard_v1::req::destroy(host_extended_id.0));
        // Unregister from the host dispatch table so stale peek_key events
        // (version ≥ 2) sent after destroy cannot be dispatched to a dead object.
        ctx.retire_destroyed_host_object(host_extended_id.0);
        log::debug!(
            "Destroyed extended keyboard: host_extended_id={} for host_keyboard_id={}",
            host_extended_id.0,
            host_keyboard_id.0
        );
    }
    crate::handler::text_input::apply_keyboard_focus_changes(
        ctx,
        &focus_update.seat_changes,
        Vec::new(),
    );
}

/// `zcr_keyboard_extension_v1` handler — sommelier only sends requests to this
/// interface (i.e. `get_extended_keyboard`); the host never sends events back to
/// the factory object, so all event callbacks are empty stubs.
impl crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::ZcrKeyboardExtensionV1Handler for KeyboardHandler {}

/// `zcr_extended_keyboard_v1` handler. Version 2's `peek_key` reports physical
/// press/release state even when the host IME consumes the corresponding key
/// and therefore omits the normal `wl_keyboard.key` event.
impl crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler for KeyboardHandler {
    fn on_peek_key(
        &mut self,
        ctx: &mut crate::state::Context,
        serial: u32,
        time: u32,
        key: u32,
        state: u32,
    ) -> crate::wire::Action {
        log::trace!(
            ">>> zcr_extended_keyboard_v1.peek_key: serial={}, time={}, key={}, state={}",
            serial,
            time,
            key,
            state
        );
        let host_extended_id = HostId::from_event_sender(ctx);
        let Some(host_keyboard_id) = ctx.keyboards.of_extended(host_extended_id) else {
            log::warn!(
                "peek_key: no host keyboard mapping for extended keyboard {}",
                host_extended_id.0
            );
            return crate::wire::Action::Drop;
        };
        let guest_seat = Self::guest_seat_for_host_keyboard(ctx, host_keyboard_id);
        // Only guest input may be recovered for an empty IME confirmation.
        // Host accelerators and placement chords belong to the host and to
        // Sommelier: if a recovery claimed one first, the wl_keyboard press
        // that follows would find the guest already owning it.
        let key_class =
            (state == KEY_STATE_PRESSED).then(|| Self::classify_key(ctx, host_keyboard_id, key));
        let input = match state {
            KEY_STATE_PRESSED => PeekInput::Press {
                serial,
                time,
                guest_input: key_class == Some(KeyClass::Guest),
            },
            KEY_STATE_RELEASED => PeekInput::Release { serial, time },
            // Exo sends no repeats.
            other => {
                log::warn!("peek_key: received unknown key state {}, ignoring", other);
                return crate::wire::Action::Drop;
            }
        };
        let outcome = ctx.key_generations.peek_key(host_keyboard_id, key, input);
        if outcome.owes_release {
            // A peek the input method's own detach made stale (H2) describes
            // a press whose release may have died with it, so a press peek
            // of the key is its next press, of a generation of its own. The
            // guest gets the old press's release first, under the serial and
            // time of the peek, which announces the new press.
            if let Some(guest_keyboard_id) = ctx.shadow_table.get_guest_id(host_keyboard_id.0) {
                ctx.host_to_client_queue.push(wl_keyboard::evt::key(
                    guest_keyboard_id,
                    serial,
                    time,
                    key,
                    KEY_STATE_RELEASED,
                ));
            }
        }
        if let Some(sequence) = outcome.numbered {
            if let Some(KeyClass::Placement(action)) = key_class {
                log::debug!(
                    "peek_key: placement chord {:?} on keyboard {}; it is placed only \
                     by the matching wl_keyboard.key, so if the host IME consumes it \
                     the chord is neither placed nor delivered",
                    action,
                    host_keyboard_id.0
                );
            }
            if let Some(guest_seat) = guest_seat {
                let focused_surface = ctx
                    .keyboard_focus
                    .surface_for_seat(guest_seat)
                    .filter(|surface| {
                        ctx.keyboard_focus
                            .keyboard_owns_surface(host_keyboard_id, *surface)
                    });
                ctx.key_generations.record_latest_peek(
                    guest_seat,
                    focused_surface,
                    host_keyboard_id,
                    sequence,
                );
            }
        }
        match input {
            // The press of another key ends the Backspace repeat lease.
            PeekInput::Press { .. } if key != EVDEV_KEY_BACKSPACE => {
                let keyboard = host_keyboard_id;
                lease(ctx, guest_seat, LeaseInput::OtherPress { keyboard });
            }
            // A Backspace release ends the Backspace repeat lease.
            PeekInput::Release { .. } if !outcome.ignored && key == EVDEV_KEY_BACKSPACE => {
                lease(ctx, guest_seat, LeaseInput::BackspaceRelease);
            }
            _ => {}
        }
        crate::wire::Action::Drop
    }
}

#[cfg(test)]
mod tests;
