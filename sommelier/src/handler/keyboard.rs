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

//! The keyboard core's adapter: `wl_keyboard` and
//! `zcr_extended_keyboard_v1` events, decoded once by the dispatcher, go to
//! [`crate::keys::KeyboardCore`] as [`KeyInput`]s, and [`keys`] applies
//! what it asks: the `ack_key` of each key, whether the guest gets it, a
//! shortcut for its subscriber, guest events, and inputs of the IME core.
//!
//! When `zcr_keyboard_extension_v1` is available from the host, the
//! proxy enables the ack-key protocol so that the core can decide per key
//! whether the host compositor (ChromeOS/Exo) should process the key as an
//! accelerator. Keys listed in `SOMMELIER_ACCELERATORS` are acked as
//! `NOT_HANDLED` (host runs the accelerator, key is not forwarded to
//! guest); all other keys are acked as `HANDLED` (guest receives the key,
//! host skips the accelerator). A chord a shortcut binds is consumed, and
//! its binding goes to the shortcut's subscriber, window placement
//! ([`crate::handler::window_placement::shortcut`]). `docs/KEYBOARD.md`
//! describes the core, and its "Host accelerators" what Exo does with an
//! acknowledgment.

use crate::accelerator::{Accelerator, BindingId};
use crate::handler::callback::queue_internal_sync;
use crate::handler::Handlers;
use crate::ime::TiId;
use crate::keys::{Ack, Env, KeyEffects, KeyInput, KeyboardFocus, Recovered};
use crate::protocols::keyboard_extension_unstable_v1::{
    zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
};
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::wayland::wl_keyboard;
use crate::protocols::AnyEvent;
use crate::protocols::Iface;
use crate::state::Singleton;
use crate::state::{
    Context, EventObject, EventSender, GuestId, HostId, InternalCallback, KeyboardModifiers,
};
use crate::warn_once::WarnOnce;
use crate::wire::{Action, ToGuest};
use std::os::fd::BorrowedFd;

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

/// The facts the keyboard core reads and does not own: the keyboard
/// focus, the shortcut bindings of its subscriber, window placement, and
/// the IME core's gate. It borrows only those, so that the core can step
/// beside them.
pub(crate) struct CtxEnv<'a> {
    placement: Option<&'a crate::state::WindowPlacement>,
    ime: &'a crate::ime::ImeCore,
}

impl<'a> CtxEnv<'a> {
    /// The facts of the context whose window placement, while on, is
    /// `placement`, and whose IME core is `ime`, borrowed apart from its
    /// keyboard core, which can then step beside them.
    pub(crate) fn new(
        placement: Option<&'a crate::state::WindowPlacement>,
        ime: &'a crate::ime::ImeCore,
    ) -> Self {
        Self { placement, ime }
    }
}

impl Env for CtxEnv<'_> {
    fn shortcut(&self, chord: &Accelerator) -> Option<BindingId> {
        self.placement?.binding_for(chord)
    }

    fn admits_recovery(&self, ti: TiId) -> bool {
        self.ime.admits_recovery(ti)
    }
}

/// Hand `input` to the keyboard core and apply its effects where the
/// message is handled, in [`KeyEffects`]' order.
///
/// # Returns
///
/// How to dispatch the host event handled, a `wl_keyboard.key` or
/// `modifiers`: forwarded if the guest gets it, unless a recovery the
/// event decided went before it, when `resend`'s copy of the event goes
/// after the recovery and the original is dropped, as a forward would
/// precede it.
pub(crate) fn deliver(
    ctx: &mut Context,
    input: KeyInput,
    resend: impl FnOnce(&Context) -> Option<ToGuest>,
) -> Action {
    let mut fx = step(ctx, input);
    let recovered = fx.recovered.is_some();
    let deliver = fx.deliver;
    apply_before(ctx, &mut fx);
    let action = match (deliver, recovered) {
        (false, _) => Action::Drop,
        (true, false) => Action::Forward,
        (true, true) => {
            ctx.send_guest_all(resend(ctx));
            Action::Drop
        }
    };
    apply_after(ctx, fx);
    action
}

/// Hand `input`, which no forward follows, to the keyboard core and apply
/// its effects ([`deliver`]).
pub(crate) fn keys(ctx: &mut Context, input: KeyInput) {
    let mut fx = step(ctx, input);
    debug_assert!(
        !fx.deliver,
        "an input with no event to forward delivers one"
    );
    apply_before(ctx, &mut fx);
    apply_after(ctx, fx);
}

/// Step the keyboard core.
fn step(ctx: &mut Context, input: KeyInput) -> KeyEffects {
    ctx.step_keys(input)
}

/// What goes before the event's own delivery: the recovery's pair and the
/// transaction that closes it, the shortcut, the acknowledgement and the
/// sync of a recovery that waits.
fn apply_before(ctx: &mut Context, fx: &mut KeyEffects) {
    if let Some(Recovered { ti, pair }) = fx.recovered.take() {
        let input = crate::ime::Input::HeldKeyRecovered { ti, pair };
        crate::handler::text_input::ime(ctx, input);
    }
    if let Some(shortcut) = fx.shortcut {
        crate::handler::window_placement::shortcut(ctx, shortcut.keyboard, shortcut.binding);
    }
    if let Some(ack) = fx.ack {
        send_ack_key(ctx, ack);
    }
    if fx.sync {
        queue_internal_sync(ctx, InternalCallback::KeyRecovery);
    }
    if let Some(host_extended_id) = fx.ended {
        ctx.send_host(zcr_extended_keyboard_v1::req::destroy(host_extended_id));
        // Unregister from the host dispatch table so stale peek_key events
        // (version ≥ 2) sent after destroy cannot be dispatched to a dead
        // object.
        ctx.retire_destroyed_host_object(host_extended_id);
        log::debug!("Destroyed extended keyboard: host_extended_id={host_extended_id}");
    }
}

/// What goes after the event's own delivery: guest events, then the IME
/// core's inputs.
fn apply_after(ctx: &mut Context, fx: KeyEffects) {
    ctx.send_guest_all(fx.to_guest);
    for input in fx.ime {
        crate::handler::text_input::ime(ctx, input);
    }
}

/// Host event `event` (none for an untracked sender), which `sender`
/// sent, is about to be handled: a waiting recovery reads it first
/// ([`KeyInput::HostEvent`]), unless its handler hands the core the event
/// itself ([`KeyInput::Key`], [`KeyInput::Modifiers`], [`KeyInput::Peek`],
/// [`KeyInput::TextInputLeave`]). What the recovery sends the guest goes
/// before the event.
///
/// A key or modifiers event the proxy drops unhandled (`suppressed`), of
/// a keyboard the guest has released, reaches no handler, so it comes
/// here ([`KeyInput::UnhandledKey`], [`KeyInput::UnhandledModifiers`]):
/// the key may be the very press a waiting recovery waits for.
pub(crate) fn before_host_event(
    ctx: &mut Context,
    event: Option<(&EventSender, &AnyEvent)>,
    suppressed: bool,
) {
    if !ctx.keys.awaits_host_event() {
        return;
    }
    let decoded_by_its_handler = match event {
        Some((sender, AnyEvent::WlKeyboard(wl_keyboard::Event::Key { .. }))) if suppressed => {
            let keyboard = sender.host;
            return keys(ctx, KeyInput::UnhandledKey { keyboard });
        }
        Some((_, AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers { .. }))) if suppressed => {
            return keys(ctx, KeyInput::UnhandledModifiers);
        }
        Some((
            _,
            AnyEvent::WlKeyboard(
                wl_keyboard::Event::Key { .. } | wl_keyboard::Event::Modifiers { .. },
            )
            | AnyEvent::ZcrExtendedKeyboardV1(zcr_extended_keyboard_v1::Event::PeekKey { .. })
            | AnyEvent::ZwpTextInputV1(zwp_text_input_v1::Event::Leave {}),
        )) => !suppressed,
        _ => false,
    };
    if !decoded_by_its_handler {
        keys(ctx, KeyInput::HostEvent);
    }
}

/// The keys a `wl_keyboard.enter` lists as held, in the host's order,
/// each key at its first listing: each key's generation is numbered in
/// this order, and a peek of a key held since the enter takes its number.
fn held_keys(keys: &[u8]) -> Vec<u32> {
    let (chunks, remainder) = keys.as_chunks::<4>();
    let mut held = Vec::with_capacity(chunks.len());
    for key in chunks.iter().map(|chunk| u32::from_ne_bytes(*chunk)) {
        if !held.contains(&key) {
            held.push(key);
        }
    }
    if !remainder.is_empty() {
        log::warn!(
            "wl_keyboard.enter keys array has {} trailing byte(s)",
            remainder.len()
        );
    }
    held
}

/// Ensure the `zcr_extended_keyboard_v1` object is bound for this keyboard.
///
/// This is idempotent: if the extended keyboard is already bound for
/// `host_keyboard_id`, this function is a no-op.
///
/// This is called from `on_enter` (rather than `wl_seat.get_keyboard`) because
/// the host keyboard ID (the sender of a host→client event) is only
/// known once we process a host event — `get_keyboard` is a client→host request
/// and at that point we only have the guest keyboard ID. `on_enter` is always
/// sent by the host before any `wl_keyboard.key` event, so this is safe:
/// the extended keyboard will be bound before the first key event.
///
/// # Ordering invariant
/// `get_extended_keyboard` is sent with the event's effects and will be
/// flushed in the same proxy loop iteration as the forwarded `wl_keyboard.enter`
/// event. Exo enables `SetNeedKeyboardKeyAcks(true)` upon processing
/// `get_extended_keyboard`. Any key events queued by the compositor *before*
/// this request is processed (e.g. a key already in flight) will not have
/// an active TTL in `pending_key_acks_`; Exo will apply its default policy for
/// those keys. This mirrors the behavior of the C sommelier reference.
pub(crate) fn ensure_extended_keyboard_bound(ctx: &mut Context, host_keyboard_id: HostId) {
    if let Some(extension_host_id) = ctx.globals.singletons().host(Singleton::KeyboardExtension) {
        let bound = ctx
            .keys
            .keyboards()
            .get(host_keyboard_id)
            .map(|k| k.extended.is_some());
        if bound == Some(false) {
            let Some(extended_version) = ctx.shadow_table.host_object_version(extension_host_id)
            else {
                let _ = ctx.fail(format_args!(
                    "keyboard extension {extension_host_id} has no version"
                ));
                return;
            };
            let host_extended_id = ctx
                .shadow_table
                .create_host_only(Iface::ZcrExtendedKeyboardV1, extended_version);
            let extended = host_extended_id;
            keys(
                ctx,
                KeyInput::ExtendedBound {
                    keyboard: host_keyboard_id,
                    extended,
                },
            );

            ctx.send_host(zcr_keyboard_extension_v1::req::get_extended_keyboard(
                extension_host_id,
                host_extended_id,
                host_keyboard_id,
            ));
            log::debug!(
                "Bound extended keyboard: host_extended_id={host_extended_id} for \
                 host_keyboard_id={host_keyboard_id}"
            );
        }
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
fn send_ack_key(ctx: &mut Context, ack: Ack) {
    let Ack {
        keyboard: host_keyboard_id,
        serial,
        handled,
    } = ack;
    let extended = ctx
        .keys
        .keyboards()
        .get(host_keyboard_id)
        .and_then(|k| k.extended);
    let Some(host_extended_id) = extended else {
        if ctx
            .globals
            .singletons()
            .get(Singleton::KeyboardExtension)
            .is_some()
        {
            log::debug!(
                "ack_key: no extended keyboard bound yet for host_keyboard_id={}; \
                     key event arrived before on_enter? serial={}, handled={}",
                host_keyboard_id,
                serial,
                handled
            );
        } else {
            log::log!(
                take_no_keyboard_extension_log_level(&NO_KEYBOARD_EXTENSION_WARNED),
                "ack_key: the host has no zcr_keyboard_extension_v1, so keys go \
                     unacknowledged (host_keyboard_id={}, serial={}, handled={}); \
                     later ones are logged at debug",
                host_keyboard_id,
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
    ctx.send_host(zcr_extended_keyboard_v1::req::ack_key(
        host_extended_id,
        serial,
        handled,
    ));
}

impl wl_keyboard::WlKeyboardHandler for Handlers {
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
        ctx: &mut Context,
        sender: EventSender,
        format: u32,
        fd: BorrowedFd<'_>,
        size: u32,
    ) -> Action {
        let keyboard = sender.host;
        let keymap = ctx.keys.keymaps().compile(format, fd, size);
        keys(ctx, KeyInput::Keymap { keyboard, keymap });
        Action::Forward
    }

    fn on_enter(
        ctx: &mut Context,
        sender: EventSender,
        serial: u32,
        surface: EventObject,
        keys: &[u8],
    ) -> Action {
        // The generated dispatcher resolved `surface` to a live guest
        // surface (`ShadowTable::event_object`): an enter of a surface the
        // guest destroyed, whose host ID stays mapped until the host's
        // `delete_id`, never reaches this handler, so it cannot resurrect
        // the surface as IME focus.
        let host_keyboard_id = sender.host;
        let Some(guest_keyboard_id) = sender.guest_id() else {
            return Action::Drop;
        };
        let guest_surface_id = surface.guest;

        log::info!(
            ">>> wl_keyboard.on_enter: host_kb={}, guest_kb={}, surface={}, guest_surface={}",
            host_keyboard_id,
            guest_keyboard_id,
            surface.host,
            guest_surface_id
        );

        let Some(guest_seat_id) = ctx.keys.keyboards().seat(host_keyboard_id) else {
            log::warn!("  -> host keyboard {host_keyboard_id} has no record");
            return Action::Drop;
        };
        let focus = KeyboardFocus {
            guest_seat: guest_seat_id,
            guest_surface: guest_surface_id,
            host_surface: surface.host,
        };
        if ctx.keys.focus().focus_for_keyboard(host_keyboard_id) == Some(focus) {
            // The C reference suppresses an enter when the resource already
            // owns this focus. Preserve physical pressed-key and XKB state as
            // well; rebuilding it from an empty/partial keys array would make
            // the later release unmatched.
            log::debug!(
                "  -> duplicate enter for keyboard {} surface {}, preserving key state",
                host_keyboard_id,
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
        ensure_extended_keyboard_bound(ctx, host_keyboard_id);

        // The protocol requires a modifiers event after every enter, but Exo
        // sends one before it, and only when the modifiers changed. Send
        // the guest the keyboard's modifiers after the enter.
        let modifiers = ctx
            .keys
            .keyboards()
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

        // The IME core sends them after the commit of a preedit the focus
        // change ends (F1). Another keyboard can enter the seat's focused
        // surface: the guest keyboard still gets its own enter, and the
        // seat's text-input focus stays as it is.
        let enter = KeyInput::Enter {
            keyboard: host_keyboard_id,
            focus,
            keys: held_keys(keys),
            events: vec![enter, modifiers],
        };
        self::keys(ctx, enter);
        Action::Drop
    }

    fn on_leave(
        ctx: &mut Context,
        sender: EventSender,
        serial: u32,
        surface: EventObject,
    ) -> Action {
        // The dispatcher resolved `surface` to a live guest surface, as for
        // `enter`.
        let host_keyboard_id = sender.host;
        let Some(guest_keyboard_id) = sender.guest_id() else {
            return Action::Drop;
        };

        log::info!(
            ">>> wl_keyboard.on_leave: host_kb={}, guest_kb={}, surface={}, guest_surface={}",
            host_keyboard_id,
            guest_keyboard_id,
            surface.host,
            surface.guest
        );

        // The IME core sends the leave after the commit of a preedit the
        // focus change ends (F1); a stale leave changes nothing.
        let leave = KeyInput::Leave {
            keyboard: host_keyboard_id,
            surface: surface.host,
            events: vec![wl_keyboard::evt::leave(
                guest_keyboard_id,
                serial,
                surface.guest,
            )],
        };
        keys(ctx, leave);
        Action::Drop
    }

    /// Hand a key to the keyboard core, which decides its `ack_key` and
    /// whether the guest gets it.
    fn on_key(
        ctx: &mut Context,
        sender: EventSender,
        serial: u32,
        time: u32,
        key: u32,
        state: u32,
    ) -> Action {
        let keyboard = sender.host;
        log::trace!(
            ">>> wl_keyboard.on_key: host_kb={}, serial={}, key={}, state={}",
            keyboard,
            serial,
            key,
            state
        );
        let input = KeyInput::Key {
            keyboard,
            serial,
            key,
            state,
        };
        let action = deliver(ctx, input, |_| {
            let guest = sender.guest_id()?;
            Some(wl_keyboard::evt::key(guest, serial, time, key, state))
        });
        log::debug!("  -> action={:?}", action);
        action
    }

    /// Track modifier state so the keyboard core resolves the correct
    /// keysym.
    fn on_modifiers(
        ctx: &mut Context,
        sender: EventSender,
        serial: u32,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) -> Action {
        log::trace!(
            ">>> wl_keyboard.on_modifiers: serial={}, depressed={:#x}, latched={:#x}, locked={:#x}, group={}",
            serial, mods_depressed, mods_latched, mods_locked, group
        );
        let keyboard = sender.host;
        let modifiers = KeyboardModifiers {
            depressed: mods_depressed,
            latched: mods_latched,
            locked: mods_locked,
            group,
        };
        let input = KeyInput::Modifiers {
            keyboard,
            modifiers,
        };
        deliver(ctx, input, |_| {
            let guest = sender.guest_id()?;
            let KeyboardModifiers {
                depressed,
                latched,
                locked,
                group,
            } = modifiers;
            Some(wl_keyboard::evt::modifiers(
                guest, serial, depressed, latched, locked, group,
            ))
        })
    }
}

/// `wl_seat.get_keyboard` of guest seat `seat` created guest keyboard
/// `keyboard`, whose host keyboard its forward has just mapped: record it.
pub(crate) fn keyboard_created(ctx: &mut Context, keyboard: GuestId, seat: GuestId) {
    match ctx.shadow_table.get_host_id(keyboard) {
        Some(host) => {
            let created = KeyInput::Created {
                keyboard: host,
                guest: keyboard,
                seat,
            };
            keys(ctx, created);
        }
        None => log::debug!("Guest keyboard {keyboard} has no host keyboard"),
    }
}

/// Forget host keyboard `host_keyboard_id`, which ended (see
/// `crate::state::lifecycle`): end its focus, its record and its key
/// state, and destroy its extended keyboard.
pub(crate) fn forget_keyboard(ctx: &mut Context, host_keyboard_id: HostId) {
    keys(ctx, KeyInput::Ended(host_keyboard_id));
}

/// `zcr_keyboard_extension_v1` handler — sommelier only sends requests to this
/// interface (i.e. `get_extended_keyboard`); the host never sends events back to
/// the factory object, so all event callbacks are empty stubs.
impl crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::ZcrKeyboardExtensionV1Handler for Handlers {}

/// `zcr_extended_keyboard_v1` handler. Version 2's `peek_key` reports physical
/// press/release state even when the host IME consumes the corresponding key
/// and therefore omits the normal `wl_keyboard.key` event.
impl crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler for Handlers {
    fn on_peek_key(
        ctx: &mut Context,
        sender: EventSender,
        serial: u32,
        time: u32,
        key: u32,
        state: u32,
    ) -> Action {
        log::trace!(
            ">>> zcr_extended_keyboard_v1.peek_key: serial={}, time={}, key={}, state={}",
            serial,
            time,
            key,
            state
        );
        let extended = sender.host;
        let input = KeyInput::Peek {
            extended,
            serial,
            time,
            key,
            state,
        };
        keys(ctx, input);
        Action::Drop
    }
}

/// What tests set up of the keyboard core through its step, as the
/// guest's and the host's messages would, where a test reaches the state
/// no peer can see.
#[cfg(test)]
pub(crate) mod for_test {
    use super::*;

    /// Record host keyboard `host` of guest keyboard `guest` on guest seat
    /// `seat`, as `wl_seat.get_keyboard` does once its forward paired them.
    pub(crate) fn created(ctx: &mut Context, guest: u32, host: u32, seat: u32) {
        let keyboard = HostId::for_test(host);
        let (guest, seat) = (GuestId::for_test(guest), GuestId::for_test(seat));
        keys(
            ctx,
            KeyInput::Created {
                keyboard,
                guest,
                seat,
            },
        );
    }

    /// Bind host keyboard `host`'s extended keyboard `extended`, as its
    /// first enter does.
    pub(crate) fn extended(ctx: &mut Context, host: u32, extended: u32) {
        let (keyboard, extended) = (HostId::for_test(host), HostId::for_test(extended));
        keys(ctx, KeyInput::ExtendedBound { keyboard, extended });
    }

    /// Focus host keyboard `host`, of guest seat `seat`, on guest surface
    /// `surface`, host surface `host_surface`, as its `wl_keyboard.enter`
    /// does, holding no key; the guest gets no event of it.
    pub(crate) fn focus(
        ctx: &mut Context,
        host: u32,
        seat: GuestId,
        surface: GuestId,
        host_surface: HostId,
    ) {
        let focus = KeyboardFocus {
            guest_seat: seat,
            guest_surface: surface,
            host_surface,
        };
        let enter = KeyInput::Enter {
            keyboard: HostId::for_test(host),
            focus,
            keys: Vec::new(),
            events: Vec::new(),
        };
        keys(ctx, enter);
    }

    /// The `peek_key` of `key` in `state` from extended keyboard
    /// `extended`.
    pub(crate) fn peek(ctx: &mut Context, extended: u32, serial: u32, key: u32, state: u32) {
        let extended = HostId::for_test(extended);
        let time = serial;
        let peek = KeyInput::Peek {
            extended,
            serial,
            time,
            key,
            state,
        };
        keys(ctx, peek);
    }
}

#[cfg(test)]
mod tests;
