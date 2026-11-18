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
//! See `sommelier/docs/KEYBOARD_SHORTCUT_INHIBITION.md` for the full protocol flow.

use crate::protocols::keyboard_extension_unstable_v1::{
    zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
};
use crate::protocols::wayland::wl_keyboard::{
    self, KEYMAP_FORMAT_NO_KEYMAP, KEYMAP_FORMAT_XKB_V1, KEY_STATE_PRESSED, KEY_STATE_RELEASED,
    KEY_STATE_REPEATED,
};
use crate::state::{
    Context, GuestKeyDelivery, GuestKeyEvent, GuestKeyOwner, HostId, KeyboardFocus,
    KeyboardModifiers,
};
use crate::window_layout::WindowLayoutAction;
use crate::wire::Action;
use std::os::fd::{AsRawFd, BorrowedFd};
use xkbcommon::xkb;

/// Linux evdev keycode reported by wl_keyboard and peek_key for Backspace.
pub(crate) const EVDEV_KEY_BACKSPACE: u32 = 14;

/// The largest `wl_keyboard.keymap` the proxy maps and compiles, in bytes.
/// Exo's keymaps are about 60 KiB. The size of a VirtWL keymap cannot be
/// checked against its file, and the proxy copies the keymap to compile it,
/// so a host size near 4 GiB would otherwise abort the process.
const MAX_KEYMAP_SIZE: u32 = 1 << 20;

/// Who owns a key, decided from its chord (see `KeyboardHandler::classify_key`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum KeyClass {
    /// Listed in `SOMMELIER_ACCELERATORS`: the host runs the accelerator.
    HostAccelerator,
    /// Bound to a placement action while the ARC bounds policy is active:
    /// Sommelier owns the chord.
    Placement(WindowLayoutAction),
    /// Ordinary guest input.
    Guest,
}

/// A private, read-only view of a keymap fd mapped into the process address space.
///
/// All `unsafe` for the mmap/munmap pair is confined here:
/// - `from_fd`: calls `mmap(MAP_PRIVATE, PROT_READ)` and stores the pointer + length.
/// - `as_bytes`: constructs a slice; valid because the mapping covers exactly `len` bytes.
/// - `Drop`: calls `munmap`; the pointer and length are never mutated after construction.
struct MmapView {
    ptr: std::ptr::NonNull<std::ffi::c_void>,
    len: usize,
}

impl MmapView {
    /// Map `len` bytes from `fd` at offset 0 as a private read-only view.
    /// Returns `None` if `len` is zero, a regular fd is shorter than `len`, or
    /// if `mmap` fails. Checking regular-file length before mapping prevents a
    /// malformed keymap event from producing a mapping that SIGBUSes when read.
    /// virtwl virtual fds intentionally report no regular-file size and are
    /// validated by the virtwl-backed mmap operation itself.
    fn from_fd(borrowed: BorrowedFd<'_>, len: usize) -> Option<Self> {
        use nix::sys::mman::{mmap, MapFlags, ProtFlags};
        use nix::sys::stat::{fstat, SFlag};

        let fd = borrowed.as_raw_fd();
        let nonzero_len = std::num::NonZeroUsize::new(len)?;
        let (file_size, file_mode) = match fstat(borrowed) {
            Ok(stat) => (stat.st_size, stat.st_mode),
            Err(error) => {
                log::error!("on_keymap: fstat failed for fd={}: {}", fd, error);
                return None;
            }
        };
        let is_regular_file = SFlag::from_bits_truncate(file_mode).contains(SFlag::S_IFREG);
        if file_size < 0 || (is_regular_file && u64::try_from(file_size).ok()? < len as u64) {
            log::error!(
                "on_keymap: fd={} ({:?}) is shorter than keymap size (mode={:#o}, fd_size={}, requested={})",
                fd,
                std::fs::read_link(format!("/proc/self/fd/{fd}")).ok(),
                file_mode,
                file_size,
                len
            );
            return None;
        }
        // Safety: fd is valid for the duration of this call; mmap does not
        // retain it. The returned pointer owns the mapping until munmap.
        let ptr = unsafe {
            mmap(
                None,
                nonzero_len,
                ProtFlags::PROT_READ,
                // Wayland requires keymap files to be mapped privately.
                // This also prevents a writable shared backing object from
                // changing the keymap while XKB is parsing it.
                MapFlags::MAP_PRIVATE,
                borrowed,
                0,
            )
            .ok()?
        };
        Some(Self { ptr, len })
    }

    /// View the mapped region as a byte slice.
    fn as_bytes(&self) -> &[u8] {
        // Safety: ptr points to `self.len` readable bytes for the lifetime of
        // self (mapping is alive until Drop); no other writer exists (PROT_READ).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr() as *const u8, self.len) }
    }
}

impl Drop for MmapView {
    fn drop(&mut self) {
        // Safety: ptr and len were set by mmap and never modified.
        // munmap can only fail with EINVAL (bad addr/len alignment), which
        // cannot happen here because ptr and len came directly from a
        // successful mmap call and are never mutated. The debug_assert
        // catches any future copy-paste of this code into a context where
        // that invariant might not hold.
        let res = unsafe { nix::sys::mman::munmap(self.ptr, self.len) };
        debug_assert!(
            res.is_ok(),
            "munmap on a valid mmap mapping must not fail: {:?}",
            res
        );
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
        let guest_keyboard_id = ctx.shadow_table.get_guest_id(host_keyboard_id.0)?;
        ctx.keyboard_to_seat.get(&guest_keyboard_id).copied()
    }

    fn clear_host_keyboard_state(ctx: &mut Context, host_keyboard_id: HostId) {
        ctx.key_generations.clear_keyboard(host_keyboard_id);
    }

    fn invalidate_peek_key_press(ctx: &mut Context, host_keyboard_id: HostId, key: u32) {
        ctx.key_generations.invalidate_peek(host_keyboard_id, key);
    }

    pub(crate) fn update_host_keyboard_key_state(
        ctx: &mut Context,
        host_keyboard_id: HostId,
        key: u32,
        state: u32,
        serial: u32,
    ) {
        ctx.key_generations
            .observe_physical_event(host_keyboard_id, key, state, Some(serial));
    }

    fn cancel_backspace_repeat(ctx: &mut Context, host_keyboard_id: HostId) {
        ctx.key_generations
            .cancel_backspace_repeat(host_keyboard_id, EVDEV_KEY_BACKSPACE);
    }

    fn initialize_host_keyboard_enter_state(
        ctx: &mut Context,
        host_keyboard_id: HostId,
        keys: &[u8],
    ) {
        let (chunks, remainder) = keys.as_chunks::<4>();
        let pressed_keys: std::collections::HashSet<_> = chunks
            .iter()
            .map(|chunk| u32::from_ne_bytes(*chunk))
            .collect();
        if !remainder.is_empty() {
            log::warn!(
                "wl_keyboard.enter keys array has {} trailing byte(s)",
                remainder.len()
            );
        }

        ctx.key_generations
            .install_enter_snapshot(host_keyboard_id, pressed_keys);
    }

    /// Resolve the chord a key forms under its keyboard's current XKB state.
    ///
    /// This is the only place a chord is computed, so host accelerators and
    /// placement bindings always match against the same value.
    ///
    /// # Returns
    ///
    /// `None` when the keyboard has no keymap yet, when the evdev keycode
    /// overflows the XKB offset, or when the key has no single keysym at the
    /// current level. Such a key is never an accelerator or a binding.
    fn current_chord(
        ctx: &Context,
        host_keyboard_id: HostId,
        key: u32,
    ) -> Option<crate::accelerator::Accelerator> {
        let state = ctx.keyboard_xkb_states.get(&host_keyboard_id)?;
        let Some(xkb_raw_keycode) = key.checked_add(8) else {
            log::warn!("Ignoring overflowing evdev keycode {}", key);
            return None;
        };
        // Use key_get_one_sym so that the full XKB state (including active shift
        // level) is considered. key_get_syms_by_level at level 0 would always
        // return the unshifted symbol, causing <Shift>-modified accelerators to
        // fail to match when Shift is held.
        //
        // key_get_one_sym returns KEY_NoSymbol when more than one keysym is
        // mapped to this key at the current shift level (e.g. dead keys or
        // Unicode combining sequences). Such a key forms no chord: we'd rather
        // forward an unknown key to the guest than accidentally suppress it.
        let sym = state.key_get_one_sym(xkb::Keycode::new(xkb_raw_keycode));
        if sym.raw() == xkb::keysyms::KEY_NoSymbol {
            return None;
        }
        Some(crate::accelerator::Accelerator {
            modifiers: Self::accelerator_modifiers(state),
            symbol: crate::accelerator::keysym_to_lower(sym.raw()),
        })
    }

    /// The modifiers `state` holds, as `accelerator.rs` masks: Control,
    /// Alt, Shift and Super, depressed or latched.
    ///
    /// A locked modifier does not count, as in C sommelier: an accelerator
    /// is a chord of held keys. Exo reports every active modifier as
    /// depressed, locks included, because its `XkbTracker` updates the mask
    /// with nothing latched or locked; Caps Lock and Num Lock are not among
    /// the four, so with Exo a lock never changes a chord either way.
    fn accelerator_modifiers(state: &xkb::State) -> u32 {
        let components = xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED;
        // Use the xkbcommon logical modifier name constants (e.g. MOD_NAME_ALT = "Mod1")
        // rather than raw X11 modifier group strings. These are the stable canonical names
        // that match across different keyboard layouts, matching C sommelier's use of
        // XKB_MOD_NAME_ALT, XKB_MOD_NAME_LOGO, etc.
        let mut modifiers = 0;
        if state.mod_name_is_active(xkb::MOD_NAME_CTRL, components) {
            modifiers |= crate::accelerator::CONTROL_MASK;
        }
        if state.mod_name_is_active(xkb::MOD_NAME_ALT, components) {
            modifiers |= crate::accelerator::ALT_MASK;
        }
        if state.mod_name_is_active(xkb::MOD_NAME_SHIFT, components) {
            modifiers |= crate::accelerator::SHIFT_MASK;
        }
        if state.mod_name_is_active(xkb::MOD_NAME_LOGO, components) {
            modifiers |= crate::accelerator::SUPER_MASK;
        }
        modifiers
    }

    /// Apply the host's `modifiers` to an XKB state.
    fn apply_modifiers(state: &mut xkb::State, modifiers: KeyboardModifiers) {
        state.update_mask(
            modifiers.depressed,
            modifiers.latched,
            modifiers.locked,
            0,
            0,
            modifiers.group,
        );
    }

    /// Decide who owns a key from its chord alone.
    ///
    /// Both key channels (`wl_keyboard.key` and `peek_key`) ask this one
    /// classification. `PlacementConfig` holds bindings only while the ARC
    /// bounds policy is active, and its validator rejects a chord that is
    /// both a host accelerator and a binding; if one slipped through, the
    /// host keeps its accelerator.
    fn classify_key(ctx: &Context, host_keyboard_id: HostId, key: u32) -> KeyClass {
        let Some(chord) = Self::current_chord(ctx, host_keyboard_id, key) else {
            return KeyClass::Guest;
        };
        let class = if ctx.is_host_accelerator(&chord) {
            KeyClass::HostAccelerator
        } else if let Some(action) = ctx.placement_action_for(&chord) {
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

    fn clear_host_keyboard_keymap(ctx: &mut Context, host_keyboard_id: HostId) {
        ctx.keyboard_keymaps.remove(&host_keyboard_id);
        ctx.keyboard_xkb_states.remove(&host_keyboard_id);
        ctx.keyboard_repeatable_keys.remove(&host_keyboard_id);
    }

    fn clear_host_keyboard_keymap_and_state(ctx: &mut Context, host_keyboard_id: HostId) {
        Self::clear_host_keyboard_keymap(ctx, host_keyboard_id);
        ctx.keyboard_keysym_to_keycode.remove(&host_keyboard_id);
        Self::clear_host_keyboard_state(ctx, host_keyboard_id);
    }

    fn keysym_to_evdev_keycodes(keymap: &xkb::Keymap) -> std::collections::HashMap<u32, u32> {
        let mut keycodes = std::collections::HashMap::new();
        for keycode_raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            if keycode_raw < 8 {
                // XKB keycodes are evdev keycodes plus eight. Do not
                // underflow on malformed/custom keymaps.
                continue;
            }
            let keycode = xkb::Keycode::new(keycode_raw);
            let layout_count = keymap.num_layouts_for_key(keycode);
            for layout in 0..layout_count {
                let level_count = keymap.num_levels_for_key(keycode, layout);
                for level in 0..level_count {
                    for keysym in keymap.key_get_syms_by_level(keycode, layout, level) {
                        // A keysym event carries the effective symbol, which
                        // may be shifted or from an alternate layout. Keep
                        // the lowest physical keycode for aliases.
                        keycodes.entry(keysym.raw()).or_insert(keycode_raw - 8);
                    }
                }
            }
        }
        keycodes
    }

    fn repeatable_evdev_keycodes(keymap: &xkb::Keymap) -> std::collections::HashSet<u32> {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .filter_map(|keycode_raw| {
                (keycode_raw >= 8 && keymap.key_repeats(xkb::Keycode::new(keycode_raw)))
                    .then_some(keycode_raw - 8)
            })
            .collect()
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
    /// this request is processed (e.g. auto-repeat already in-flight) will not have
    /// an active TTL in `pending_key_acks_`; Exo will apply its default policy for
    /// those keys. This mirrors the behavior of the C sommelier reference.
    pub(crate) fn ensure_extended_keyboard_bound(ctx: &mut Context, host_keyboard_id: HostId) {
        if let Some(extension_host_id) = ctx.host_keyboard_extension_id {
            if !ctx
                .keyboard_to_extended_keyboard
                .contains_key(&host_keyboard_id)
            {
                let extended_version = ctx
                    .shadow_table
                    .host_object_version_or_unknown(extension_host_id.0);
                let host_extended_id = HostId(
                    ctx.shadow_table
                        .create_host_only("zcr_extended_keyboard_v1", extended_version),
                );
                ctx.keyboard_to_extended_keyboard
                    .insert(host_keyboard_id, host_extended_id);
                ctx.extended_keyboard_to_keyboard
                    .insert(host_extended_id, host_keyboard_id);

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

    /// Send zcr_extended_keyboard_v1.ack_key to the host.
    ///
    /// The manager global may disappear after a child has been created. The
    /// child has its own protocol lifetime, so route acknowledgements from the
    /// child mapping directly and do not require the manager binding to remain.
    /// If no child has been bound for this keyboard, the key event arrived
    /// before `on_enter` (or the host has no extension), so no acknowledgement
    /// is sent.
    fn send_ack_key(ctx: &mut Context, host_keyboard_id: HostId, serial: u32, handled: bool) {
        let Some(&host_extended_id) = ctx.keyboard_to_extended_keyboard.get(&host_keyboard_id)
        else {
            log::warn!(
                "ack_key: no extended keyboard bound for host_keyboard_id={}; \
                 key event arrived before on_enter? serial={}, handled={}",
                host_keyboard_id.0,
                serial,
                handled
            );
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
    /// Parse the keymap to set up XKB state for keysym resolution.
    /// The host sends keymap data via a shared-memory fd (e.g. memfd).
    /// We mmap it to read without consuming data, so the fd can still
    /// be forwarded to the guest client.
    fn on_keymap(
        &mut self,
        ctx: &mut Context,
        format: u32,
        fd: BorrowedFd<'_>,
        size: u32,
    ) -> Action {
        let host_keyboard_id = HostId::from_event_sender(ctx);
        if format == KEYMAP_FORMAT_NO_KEYMAP {
            Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }
        // Only handle XKB_V1 format keymaps.
        if format != KEYMAP_FORMAT_XKB_V1 {
            log::warn!("on_keymap: unsupported keymap format {}, ignoring", format);
            Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }

        // A zero-size keymap is malformed; mmap(len=0) is UB per POSIX.
        if size == 0 {
            log::warn!("on_keymap: received zero-size keymap from host, ignoring");
            Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }
        if size > MAX_KEYMAP_SIZE {
            log::warn!(
                "on_keymap: keymap of {size} bytes exceeds the {MAX_KEYMAP_SIZE}-byte limit, ignoring"
            );
            Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }

        // size is a u32 from the Wayland wire; the cast to usize is lossless on
        // 64-bit Linux (the only supported target for sommelier).
        let Some(mapping) = MmapView::from_fd(fd, size as usize) else {
            log::error!(
                "on_keymap: mmap failed for fd={}, size={}",
                fd.as_raw_fd(),
                size
            );
            Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        };
        let slice = mapping.as_bytes();

        // Per the Wayland spec, wl_keyboard.keymap.size includes a trailing NUL.
        // A malformed host event must not panic the proxy or leave stale XKB
        // state active for this keyboard.
        if slice.last() != Some(&0) {
            log::error!(
                "on_keymap: keymap data is missing the trailing NUL required by the Wayland spec"
            );
            Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }
        let len = slice.len() - 1;

        match std::str::from_utf8(&slice[..len]) {
            Err(e) => {
                log::error!("on_keymap: keymap bytes are not valid UTF-8: {}", e);
                // Clear keymap, state, and drop set together: all three must
                // remain mutually consistent. Leaving `keymap` set while `state`
                // is None creates a split where future code reading `keymap`
                // operates on stale data with no active XKB state to validate
                // against.
                Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            }
            Ok(s) => match xkb::Keymap::new_from_string(
                &self.context,
                // `new_from_string` takes ownership; copy required because `s`
                // borrows from the mmap region which is dropped at the end of
                // this function.
                s.to_owned(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) {
                None => {
                    log::error!("on_keymap: xkbcommon failed to compile the keymap string");
                    // Clear keymap, state, and drop set together for the same
                    // consistency reason as the UTF-8 error case above: all
                    // three must remain in sync.
                    Self::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
                }
                Some(keymap) => {
                    // Replacing the XKB keymap must not end the current
                    // physical-key session. Wayland may resend a keymap while
                    // keys are held; clearing the forwarded/pressed sets here
                    // would make the later releases disappear and leave the
                    // guest with stuck keys. Reset only XKB interpretation
                    // state and accelerator drop bookkeeping.
                    Self::clear_host_keyboard_keymap(ctx, host_keyboard_id);
                    ctx.keyboard_keysym_to_keycode.remove(&host_keyboard_id);
                    let keysym_map = Self::keysym_to_evdev_keycodes(&keymap);
                    let repeatable_keys = Self::repeatable_evdev_keycodes(&keymap);
                    let mut state = xkb::State::new(&keymap);
                    // Exo sends only the keymap on a layout change, so the
                    // modifiers held and locked before it still apply.
                    if let Some(&modifiers) = ctx.keyboard_modifiers.get(&host_keyboard_id) {
                        Self::apply_modifiers(&mut state, modifiers);
                    }
                    ctx.keyboard_xkb_states.insert(host_keyboard_id, state);
                    ctx.keyboard_keymaps.insert(host_keyboard_id, keymap);
                    ctx.keyboard_keysym_to_keycode
                        .insert(host_keyboard_id, keysym_map);
                    ctx.keyboard_repeatable_keys
                        .insert(host_keyboard_id, repeatable_keys);
                    // Clear stale drop state: a key dropped under the old
                    // keymap may map to a different keysym under the new one,
                    // and a forgotten drop entry would cause a stuck key.
                    ctx.key_generations
                        .clear_accelerator_suppressions(host_keyboard_id);
                    log::info!("XKB keymap loaded successfully");
                }
            },
        }
        // `mapping` drops here, unmapping the region.

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

        let Some(&guest_seat_id) = ctx.keyboard_to_seat.get(&guest_keyboard_id) else {
            log::warn!(
                "  -> guest_kb {} not in keyboard_to_seat map",
                guest_keyboard_id
            );
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
            Self::clear_host_keyboard_state(ctx, *retired_keyboard);
        }
        Self::initialize_host_keyboard_enter_state(ctx, host_keyboard_id, keys);
        // The protocol requires a modifiers event after every enter, but Exo
        // sends one before it, and only when the modifiers changed. Send
        // the guest the keyboard's modifiers after the forwarded enter.
        let modifiers = ctx
            .keyboard_modifiers
            .get(&host_keyboard_id)
            .copied()
            .unwrap_or_default();
        ctx.host_to_client_queue.push(wl_keyboard::evt::modifiers(
            guest_keyboard_id,
            serial,
            modifiers.depressed,
            modifiers.latched,
            modifiers.locked,
            modifiers.group,
        ));

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
            return Action::Forward;
        }
        crate::handler::text_input::apply_keyboard_focus_changes(ctx, &focus_update.seat_changes);

        Action::Forward
    }

    fn on_leave(&mut self, ctx: &mut Context, _serial: u32, surface: u32) -> Action {
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
            Self::clear_host_keyboard_state(ctx, *retired_keyboard);
        }
        crate::handler::text_input::apply_keyboard_focus_changes(ctx, &focus_update.seat_changes);

        if surface_can_be_forwarded {
            Action::Forward
        } else {
            Action::Drop
        }
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
        let peek_press_serial = ctx.key_generations.peek_press_serial(host_keyboard_id, key);
        if state == KEY_STATE_RELEASED
            && ctx
                .key_generations
                .take_pending_physical_release(host_keyboard_id, key, serial)
        {
            // A newer physical generation may already be current while the
            // release paired with an older peek arrives late. Close only the
            // retired guest press and leave current physical state untouched.
            Self::send_ack_key(ctx, host_keyboard_id, serial, true);
            return Action::Forward;
        }
        if state == KEY_STATE_RELEASED
            && peek_press_serial.is_some()
            && ctx
                .key_generations
                .physical_release_serial(host_keyboard_id, key)
                != Some(serial)
        {
            // The extension delivers each peek release before the matching
            // wl_keyboard release with the same serial. A non-matching release
            // belongs to another generation and must not close this one.
            Self::send_ack_key(ctx, host_keyboard_id, serial, false);
            return Action::Drop;
        }
        if state == KEY_STATE_PRESSED
            && ctx.key_generations.physically_held(host_keyboard_id, key)
            && peek_press_serial.is_some_and(|peek_serial| peek_serial != serial)
        {
            // peek_key promises that the following wl_keyboard event has the
            // same serial. A different press while this peek generation is
            // held belongs to an older/stale channel and cannot open another
            // guest press.
            Self::send_ack_key(ctx, host_keyboard_id, serial, false);
            return Action::Drop;
        }
        let backspace_repeat_active = key == EVDEV_KEY_BACKSPACE
            && crate::handler::text_input::backspace_repeat_active_for_keyboard(
                ctx,
                host_keyboard_id,
            );
        if state == KEY_STATE_PRESSED {
            let delayed_released_peek_press =
                ctx.key_generations.physical_released(host_keyboard_id, key)
                    && peek_press_serial == Some(serial);
            if !delayed_released_peek_press {
                // A press after an observed release starts a new generation.
                // The matching press for an earlier peek is a delayed channel,
                // not a new physical boundary.
                Self::update_host_keyboard_key_state(ctx, host_keyboard_id, key, state, serial);
            }
        }
        if state == KEY_STATE_RELEASED
            || (state == KEY_STATE_REPEATED
                && matches!(
                    ctx.guest_key_owner(host_keyboard_id, key),
                    Some(
                        GuestKeyOwner::Physical
                            | GuestKeyOwner::TextInputKeysym
                            | GuestKeyOwner::CompositorShortcut
                    )
                ))
        {
            Self::update_host_keyboard_key_state(ctx, host_keyboard_id, key, state, serial);
        }

        let guest_owner = ctx.guest_key_owner(host_keyboard_id, key);
        let key_class = match state {
            KEY_STATE_PRESSED | KEY_STATE_REPEATED => {
                Self::classify_key(ctx, host_keyboard_id, key)
            }
            _ => KeyClass::Guest,
        };
        // Only the press that claims an unowned key generation for a
        // placement chord is routed here, because it runs placement (a side
        // effect) first. A duplicate press, a repeat and a release are sent
        // as physical events: the reducer's `arbitrate` routes them to the
        // channel that owns the generation (see
        // `GuestKeyOwner::admits_compositor_shortcut`).
        let claims_shortcut = match (state, guest_owner, key_class) {
            (KEY_STATE_PRESSED, None, KeyClass::Placement(action)) => {
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
                true
            }
            _ => false,
        };

        let decision = match state {
            KEY_STATE_PRESSED | KEY_STATE_REPEATED => {
                let repeated = state == KEY_STATE_REPEATED;
                if key != EVDEV_KEY_BACKSPACE {
                    Self::cancel_backspace_repeat(ctx, host_keyboard_id);
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
                // A Sommelier-owned shortcut is not guest input, and neither
                // is a host accelerator: neither may become an IME recovery
                // candidate if peek_key raced ahead of this event.
                let compositor_shortcut =
                    claims_shortcut || guest_owner == Some(GuestKeyOwner::CompositorShortcut);
                let host_accelerator =
                    !compositor_shortcut && key_class == KeyClass::HostAccelerator;
                if compositor_shortcut || host_accelerator {
                    Self::invalidate_peek_key_press(ctx, host_keyboard_id, key);
                }
                let event = if claims_shortcut {
                    GuestKeyEvent::CompositorShortcutPress
                } else {
                    GuestKeyEvent::PhysicalPress {
                        repeated,
                        host_accelerator,
                        ime_repeat_active: backspace_repeat_active,
                    }
                };
                ctx.transition_guest_key(host_keyboard_id, key, event)
            }
            KEY_STATE_RELEASED => {
                let decision =
                    ctx.transition_guest_key(host_keyboard_id, key, GuestKeyEvent::PhysicalRelease);
                if key == EVDEV_KEY_BACKSPACE && decision.ends_repeat {
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
                decision
            }
            other => {
                log::warn!("on_key: received unknown key state {}, ignoring", other);
                return Action::Drop;
            }
        };

        let handled = decision
            .ack_handled
            .expect("physical key decisions always carry an ACK");
        Self::send_ack_key(ctx, host_keyboard_id, serial, handled);
        let action = match decision.delivery {
            GuestKeyDelivery::Forward => Action::Forward,
            GuestKeyDelivery::Drop => Action::Drop,
            GuestKeyDelivery::EmitBalancedPair => {
                unreachable!("physical key events never emit synthetic pairs")
            }
        };

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
        ctx.keyboard_modifiers.insert(host_keyboard_id, modifiers);
        if let Some(state) = ctx.keyboard_xkb_states.get_mut(&host_keyboard_id) {
            Self::apply_modifiers(state, modifiers);
        }
        Action::Forward
    }
}

/// Forget guest keyboard `guest_keyboard_id` and its host keyboard
/// `host_keyboard_id`, which ended (see `crate::state::lifecycle`): end its
/// focus, destroy its extended keyboard, and clear its routing, keymap, XKB
/// and key state.
pub(crate) fn forget_keyboard(ctx: &mut Context, guest_keyboard_id: u32, host_keyboard_id: HostId) {
    ctx.keyboard_to_seat.remove(&guest_keyboard_id);
    let focus_update = ctx.keyboard_focus.release(host_keyboard_id);
    // Clear per-keyboard keymap, dropped keys, XKB state, and modifiers.
    // All must be reset so that a re-created keyboard starts from a clean
    // slate and doesn't inherit stale state from the previous session.
    //
    // accelerator suppression: a key dropped under the old session would
    //   cause its release event to be silently swallowed on the new keyboard.
    // modifiers: if the new keyboard receives a key event before the first
    //   wl_keyboard.modifiers, the accelerator check would use stale modifier
    //   bits and could produce wrong NOT_HANDLED/HANDLED decisions. Only the
    //   keyboard's end clears them; see `Context::keyboard_modifiers`.
    KeyboardHandler::clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
    ctx.keyboard_modifiers.remove(&host_keyboard_id);
    if let Some(host_extended_id) = ctx.keyboard_to_extended_keyboard.remove(&host_keyboard_id) {
        ctx.extended_keyboard_to_keyboard.remove(&host_extended_id);
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
    crate::handler::text_input::apply_keyboard_focus_changes(ctx, &focus_update.seat_changes);
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
        let Some(&host_keyboard_id) = ctx
            .extended_keyboard_to_keyboard
            .get(&host_extended_id)
        else {
            log::warn!(
                "peek_key: no host keyboard mapping for extended keyboard {}",
                host_extended_id.0
            );
            return crate::wire::Action::Drop;
        };
        let guest_seat = Self::guest_seat_for_host_keyboard(ctx, host_keyboard_id);
        let pressed_before = ctx
            .key_generations
            .physically_held(host_keyboard_id, key);
        let peek_pressed_before =
            pressed_before && ctx.key_generations.peek(host_keyboard_id, key).is_some();
        match state {
            KEY_STATE_PRESSED | KEY_STATE_REPEATED
                if state == KEY_STATE_PRESSED || pressed_before =>
            {
                if state == KEY_STATE_PRESSED && !peek_pressed_before {
                    let already_dropped_as_accelerator = ctx
                        .key_generations
                        .host_accelerator_suppressed(host_keyboard_id, key);
                    // peek_key may trail the wl_keyboard press it describes.
                    // A generation already consumed by a compositor shortcut
                    // must not become an IME recovery candidate, even if a
                    // reload has since removed its binding.
                    // Only a still-held generation is the one this peek
                    // describes; a released one is retired by the new press.
                    let consumed_by_shortcut = pressed_before
                        && ctx.guest_key_owner(host_keyboard_id, key)
                            == Some(GuestKeyOwner::CompositorShortcut);
                    // Only guest input may be recovered or delivered through
                    // the IME channel. Host accelerators and placement chords
                    // belong to the host and to Sommelier: if text-input
                    // claimed one first, the wl_keyboard press that follows
                    // would find the guest already owning it.
                    let key_class = Self::classify_key(ctx, host_keyboard_id, key);
                    if let KeyClass::Placement(action) = key_class {
                        log::debug!(
                            "peek_key: placement chord {:?} on keyboard {}; it is placed only \
                             by the matching wl_keyboard.key, so if the host IME consumes it \
                             the chord is neither placed nor delivered",
                            action,
                            host_keyboard_id.0
                        );
                    }
                    let eligible = !already_dropped_as_accelerator
                        && !consumed_by_shortcut
                        && key_class == KeyClass::Guest;
                    let sequence = ctx.key_generations.observe_peek_press(
                        host_keyboard_id,
                        key,
                        serial,
                        time,
                        eligible,
                    );
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
                } else if state == KEY_STATE_REPEATED
                    || (state == KEY_STATE_PRESSED && peek_pressed_before)
                {
                    // Both protocol v10 repeated and legacy duplicate pressed
                    // events describe the existing generation.
                    ctx.key_generations
                        .refresh_peek(host_keyboard_id, key, serial, time);
                }
                Self::update_host_keyboard_key_state(ctx, host_keyboard_id, key, state, serial);
                if state == KEY_STATE_PRESSED && key != EVDEV_KEY_BACKSPACE {
                    Self::cancel_backspace_repeat(ctx, host_keyboard_id);
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
            }
            KEY_STATE_REPEATED => {
                log::warn!(
                    "peek_key: ignoring repeated key {} without a preceding press",
                    key
                );
            }
            KEY_STATE_RELEASED => {
                if let Some(press) = ctx.key_generations.peek(host_keyboard_id, key) {
                    // ChromeOS may reuse one Wayland serial across many
                    // physical key transitions. A newer serial is sufficient
                    // to order the release; when the serial is reused, the
                    // compositor timestamp orders events within that serial.
                    // Do not accept a release merely because its timestamp is
                    // newer: an old serial can arrive late with an unrelated
                    // timestamp.
                    let valid_release = crate::state::serial_is_after(serial, press.serial)
                        || (serial == press.serial
                            && !crate::state::serial_is_after(press.time, time));
                    if !valid_release {
                        log::debug!(
                            "peek_key: dropping stale release serial={} time={} before \
                             current serial={} time={}",
                            serial,
                            time,
                            press.serial,
                            press.time,
                        );
                        return crate::wire::Action::Drop;
                    }
                }
                ctx.key_generations
                    .observe_peek_release(host_keyboard_id, key, serial);
                // Keep an IME-recovery owner as a completed-generation
                // tombstone until the next physical press. Text-input keysym
                // events may lag behind this release and must remain
                // suppressible.
                if key == EVDEV_KEY_BACKSPACE {
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
            }
            other => {
                log::warn!("peek_key: received unknown key state {}, ignoring", other);
            }
        }
        crate::wire::Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc_task_ids::ArcIdAllocator;
    use crate::handler::window_placement::{place_focused, PlacementSkip};
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_surface, zaura_toplevel};
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::{
        ZcrExtendedKeyboardV1Handler, HANDLED_STATE_HANDLED as HANDLED,
        HANDLED_STATE_NOT_HANDLED as NOT_HANDLED,
    };
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::{
        self, ZcrExtendedTextInputV1Handler,
    };
    use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::{
        self, ZwpTextInputV1Handler,
    };
    use crate::protocols::text_input_unstable_v3::{zwp_text_input_manager_v3, zwp_text_input_v3};
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::protocols::wayland::{wl_display, wl_registry, wl_seat};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session};
    use crate::state::{GuestKeyEvent, OutputState};
    use crate::test_support::{decode, msg_opcode, msg_sender, shm_pool_file};
    use crate::wire::WL_DISPLAY_ID;
    use std::os::fd::AsFd;
    use std::sync::Arc;
    use xkb::keysyms;
    use GuestKeyOwner::{CompositorShortcut, ImeRecovery, Physical, TextInputKeysym};
    use WindowLayoutAction::TopLeft;

    // Evdev keycodes and modifier masks of the default keymap, which
    // `keymap_loads_from_non_rewound_memfd` pins.
    const KEY_Q: u32 = 16;
    const KEY_W: u32 = 17;
    const KEY_E: u32 = 18;
    const KEY_T: u32 = 20;
    const KEY_A: u32 = 30;
    const KEY_S: u32 = 31;
    const KEY_D: u32 = 32;
    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_Z: u32 = 44;
    const KEY_X: u32 = 45;
    const KEY_C: u32 = 46;
    const KEY_B: u32 = 48;
    const KEY_SPACE: u32 = 57;
    const BACKSPACE: u32 = EVDEV_KEY_BACKSPACE;
    const SHIFT: u32 = 1 << 0;
    const CAPS_LOCK: u32 = 1 << 1;
    const CONTROL: u32 = 1 << 2;
    const ALT: u32 = 1 << 3;

    const PRESSED: u32 = KEY_STATE_PRESSED;
    const RELEASED: u32 = KEY_STATE_RELEASED;
    const REPEATED: u32 = KEY_STATE_REPEATED;

    // -----------------------------------------------------------------------
    // Handler fixture: for state no peer sees
    // -----------------------------------------------------------------------

    /// A guest keyboard of guest seat `seat`, and the host keyboard and
    /// `zcr_extended_keyboard_v1` the proxy pairs with it.
    #[derive(Clone, Copy)]
    struct Mapped {
        guest: u32,
        host: u32,
        extended: u32,
        seat: u32,
    }

    const KEYBOARD: Mapped = Mapped {
        guest: 10,
        host: 100,
        extended: 1000,
        seat: 1,
    };

    /// Another keyboard of `KEYBOARD`'s seat.
    const OTHER_KEYBOARD: Mapped = Mapped {
        guest: 11,
        host: 101,
        extended: 1001,
        seat: 1,
    };

    /// A guest text input, and the host `zwp_text_input_v1` and
    /// `zcr_extended_text_input_v1` behind it.
    #[derive(Clone, Copy)]
    struct MappedTextInput {
        guest: u32,
        host: u32,
        extended: u32,
    }

    const TEXT_INPUT: MappedTextInput = MappedTextInput {
        guest: 40,
        host: 140,
        extended: 2000,
    };

    /// A surface the tests focus that the guest never created.
    const UNCREATED_SURFACE: u32 = 900;

    /// The keyboard handler and the proxy state it works on. Each method
    /// dispatches one event as the generated dispatcher would, from the
    /// sender it names.
    struct Proxy {
        handler: KeyboardHandler,
        ctx: Context,
    }

    impl Proxy {
        fn new() -> Self {
            Self::with(Context::new_for_test(false, false, Vec::new()))
        }

        fn with(ctx: Context) -> Self {
            Self {
                handler: KeyboardHandler::new(),
                ctx,
            }
        }

        /// Pair keyboard `k` as `wl_seat.get_keyboard` and its first enter
        /// do. Backspace repeats, as a keymap would make it.
        fn map(&mut self, k: Mapped) {
            let ctx = &mut self.ctx;
            ctx.shadow_table
                .insert_paired_for_test(k.guest, k.host, "wl_keyboard");
            ctx.keyboard_to_seat.insert(k.guest, k.seat);
            ctx.keyboard_to_extended_keyboard
                .insert(HostId(k.host), HostId(k.extended));
            ctx.extended_keyboard_to_keyboard
                .insert(HostId(k.extended), HostId(k.host));
            ctx.keyboard_repeatable_keys
                .entry(HostId(k.host))
                .or_default()
                .insert(BACKSPACE);
        }

        /// Pair guest surface `guest` with host surface `host`.
        fn surface(&mut self, guest: u32, host: u32) {
            self.ctx
                .shadow_table
                .insert_paired_for_test(guest, host, "wl_surface");
        }

        /// Record that `k`'s focus is on guest surface `surface`.
        fn focus(&mut self, k: Mapped, surface: u32) {
            let host_surface = self
                .ctx
                .shadow_table
                .get_host_id(surface)
                .unwrap_or(surface);
            self.ctx
                .keyboard_focus
                .set_for_test(HostId(k.host), k.seat, surface, host_surface);
        }

        /// Text input `t` of `seat`, enabled with surrounding text and
        /// active on the host, on guest surface `surface`.
        fn text_input(&mut self, t: MappedTextInput, seat: u32, surface: u32) {
            self.ctx
                .shadow_table
                .insert_paired_for_test(t.guest, t.host, "zwp_text_input_v3");
            let mut state =
                crate::state::TextInputState::new(t.host, Some(t.extended), seat, Some(surface));
            state.committed.enabled = true;
            state.committed.surrounding_text = Some(("가".to_string(), 3, 3));
            state.set_guest_commits(1);
            state.host = crate::state::HostApplied::Active { content_type: None };
            self.ctx.text_inputs.insert(t.guest, state);
        }

        /// `k`'s host sends a keymap event; returns what the handler does.
        fn keymap_of(&mut self, k: Mapped, format: u32, bytes: &[u8], size: u32) -> Action {
            self.ctx.last_sender_id = k.host;
            // The memfd's offset stays at its end, as a host may leave it.
            let file = shm_pool_file(bytes);
            self.handler
                .on_keymap(&mut self.ctx, format, file.as_fd(), size)
        }

        /// `k`'s host sends the default keymap, which loads.
        fn keymap(&mut self, k: Mapped) {
            let keymap = crate::proxy::harness::default_keymap();
            let size = keymap.len() as u32;
            let action = self.keymap_of(k, KEYMAP_FORMAT_XKB_V1, &keymap, size);
            assert_eq!(action, Action::Forward);
            assert!(self.ctx.keyboard_keymaps.contains_key(&HostId(k.host)));
        }

        /// `k`'s host holds the `depressed` modifiers.
        fn modifiers(&mut self, k: Mapped, depressed: u32) {
            self.ctx.last_sender_id = k.host;
            let action = self
                .handler
                .on_modifiers(&mut self.ctx, 1, depressed, 0, 0, 0);
            assert_eq!(action, Action::Forward);
        }

        fn key(&mut self, k: Mapped, serial: u32, time: u32, key: u32, state: u32) -> Action {
            self.ctx.last_sender_id = k.host;
            self.handler.on_key(&mut self.ctx, serial, time, key, state)
        }

        fn peek(&mut self, k: Mapped, serial: u32, time: u32, key: u32, state: u32) -> Action {
            self.ctx.last_sender_id = k.extended;
            self.handler
                .on_peek_key(&mut self.ctx, serial, time, key, state)
        }

        /// `k`'s host enters host surface `surface` with `keys` held.
        fn enter(&mut self, k: Mapped, surface: u32, keys: &[u32]) -> Action {
            self.ctx.last_sender_id = k.host;
            let keys: Vec<u8> = keys.iter().flat_map(|key| key.to_ne_bytes()).collect();
            self.handler.on_enter(&mut self.ctx, 1, surface, &keys)
        }

        /// `k`'s host leaves host surface `surface`.
        fn leave(&mut self, k: Mapped, surface: u32) -> Action {
            self.ctx.last_sender_id = k.host;
            self.handler.on_leave(&mut self.ctx, 1, surface)
        }

        /// The guest releases `k`, as the generated dispatcher does: the
        /// handler forwards `wl_keyboard.release`, and ending the keyboard
        /// forgets its state.
        fn release(&mut self, k: Mapped) {
            self.ctx.last_sender_id = k.guest;
            assert_eq!(self.handler.on_release(&mut self.ctx), Action::Forward);
            self.ctx.guest_destroyed(k.guest);
        }

        /// The host input method of `t` confirms an empty composition.
        fn confirm(&mut self, t: MappedTextInput) -> Action {
            self.ctx.last_sender_id = t.extended;
            crate::handler::text_input::ExtendedTextInputV1Handler
                .on_confirm_preedit(&mut self.ctx, 0)
        }

        /// The host input method of `t` sends a keysym. Exo swaps the
        /// protocol's field names: `serial` carries the timestamp and `time`
        /// the Wayland serial.
        fn keysym(
            &mut self,
            t: MappedTextInput,
            time: u32,
            serial: u32,
            sym: u32,
            state: u32,
        ) -> Action {
            self.ctx.last_sender_id = t.host;
            crate::handler::text_input::TextInputV1Handler.on_keysym(
                &mut self.ctx,
                time,
                serial,
                sym,
                state,
                0,
            )
        }

        fn owner(&self, k: Mapped, key: u32) -> Option<GuestKeyOwner> {
            self.ctx.guest_key_owner(HostId(k.host), key)
        }

        fn held(&self, k: Mapped, key: u32) -> bool {
            self.ctx
                .key_generations
                .physically_held(HostId(k.host), key)
        }

        fn cancelled(&self, k: Mapped) -> bool {
            self.ctx
                .key_generations
                .backspace_repeat_cancelled(HostId(k.host), BACKSPACE)
        }

        fn classify(&self, k: Mapped, key: u32) -> KeyClass {
            KeyboardHandler::classify_key(&self.ctx, HostId(k.host), key)
        }
    }

    // -----------------------------------------------------------------------
    // Session fixture: for what the peers see
    // -----------------------------------------------------------------------

    /// A guest keyboard in a [`Session`], and the host objects behind it.
    #[derive(Clone, Copy, Debug)]
    struct Keyboard {
        id: u32,
        host: u32,
        /// The host `zcr_extended_keyboard_v1` the proxy created for the
        /// keyboard at its first enter; 0 before.
        ext: u32,
    }

    /// A guest text input the host activated, and the host
    /// `zwp_text_input_v1` and `zcr_extended_text_input_v1` behind it.
    #[derive(Clone, Copy, Debug)]
    struct Ime {
        id: u32,
        host: u32,
        ext: u32,
        /// The host `wl_seat` the host text input is activated for.
        host_seat: u32,
    }

    /// Where a host key event went, from what each peer received.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Route {
        /// The guest got it, and the host's ack says the guest handled it.
        Guest,
        /// The guest did not, and the host's ack lets the host handle it.
        Host,
        /// The guest did not, and the host's ack says it was handled: the
        /// proxy consumed it, as a duplicate press or a compositor shortcut.
        Consumed,
    }

    /// A whole proxy session whose host has the keyboard extension, with a
    /// guest seat, keyboard `k` of it with the default keymap, and a
    /// surface.
    struct Kb {
        s: Session,
        /// The name of the extension's global.
        extension: u32,
        seat: u32,
        k: Keyboard,
        surface: Object,
    }

    impl Kb {
        /// `k`'s focus has not entered the surface.
        async fn unfocused(ctx: Context) -> Self {
            let mut s = Session::with_context(ctx);
            let extension = s.advertise(
                "zcr_keyboard_extension_v1",
                crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::VERSION,
            )
            .await;
            // The proxy binds the extension for itself.
            let bind = s.host_received();
            assert!(
                matches!(
                    &bind[..],
                    [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { .. }))]
                ),
                "{bind:?}"
            );
            let (seat, k) = s.keyboard().await;
            let surface = s.surface().await;
            let k = Keyboard {
                id: k.id,
                host: k.host,
                ext: 0,
            };
            Self {
                s,
                extension,
                seat,
                k,
                surface,
            }
        }

        /// `k`'s focus is on the surface.
        async fn new() -> Self {
            Self::focused(Context::new_for_test(false, false, Vec::new())).await
        }

        /// [`Self::new`] with host accelerators `spec`.
        async fn with_accelerators(spec: &str) -> Self {
            let accelerators = crate::accelerator::parse_accelerators(spec).unwrap();
            Self::focused(Context::new_for_test(false, false, accelerators)).await
        }

        async fn focused(ctx: Context) -> Self {
            let mut kb = Self::unfocused(ctx).await;
            let surface = kb.surface;
            kb.k = kb.first_enter(kb.k, surface).await;
            kb
        }

        /// The host gives `k`, which holds no modifiers, its first focus,
        /// on `surface`; returns `k` with the extended keyboard the proxy
        /// creates for it.
        async fn first_enter(&mut self, k: Keyboard, surface: Object) -> Keyboard {
            let enter = wl_keyboard::evt::enter(k.host, 1, surface.host, &[]).unwrap();
            self.s.host([enter]).await;
            let k = Keyboard {
                ext: extended_keyboard_of(k, &self.s.host_received()),
                ..k
            };
            assert_eq!(
                self.s.guest_received(),
                [entered(k, 1, surface.id, &[]), modifiers(k, 1, 0)]
            );
            k
        }

        /// Another keyboard of the seat, which the host gives its first
        /// focus on the surface, which the seat's focus is on.
        async fn another_keyboard(&mut self) -> Keyboard {
            let k = self.s.keyboard_of(self.seat).await;
            let k = Keyboard {
                id: k.id,
                host: k.host,
                ext: 0,
            };
            let surface = self.surface;
            self.first_enter(k, surface).await
        }

        /// `k`'s host sends key event `serial` at time `serial`; returns
        /// where it went. `on_key` ignores the time, so the tests that need
        /// one pass it to a peek.
        async fn key_on(&mut self, k: Keyboard, serial: u32, key: u32, state: u32) -> Route {
            self.s
                .host([wl_keyboard::evt::key(k.host, serial, serial, key, state)])
                .await;
            let guest = self.s.guest_received();
            let host = self.s.host_received();
            let delivered = [key_event(k, serial, serial, key, state)];
            match (guest == delivered, guest.is_empty(), &host[..]) {
                (true, _, [ack]) if *ack == acked(k, serial, HANDLED) => Route::Guest,
                (_, true, [ack]) if *ack == acked(k, serial, NOT_HANDLED) => Route::Host,
                (_, true, [ack]) if *ack == acked(k, serial, HANDLED) => Route::Consumed,
                _ => panic!("key {serial} went astray: the guest got {guest:?}, the host {host:?}"),
            }
        }

        async fn key(&mut self, serial: u32, key: u32, state: u32) -> Route {
            self.key_on(self.k, serial, key, state).await
        }

        /// `k`'s extended keyboard peeks a key, which reaches no peer.
        async fn peek_on(&mut self, k: Keyboard, serial: u32, time: u32, key: u32, state: u32) {
            use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
            self.s
                .host([zcr_extended_keyboard_v1::evt::peek_key(
                    k.ext, serial, time, key, state,
                )])
                .await;
            assert_eq!(self.s.guest_received(), []);
            assert_eq!(self.s.host_received(), []);
        }

        async fn peek(&mut self, serial: u32, time: u32, key: u32, state: u32) {
            self.peek_on(self.k, serial, time, key, state).await;
        }

        /// `k`'s host holds the `depressed` modifiers, which the guest gets.
        async fn modifiers_on(&mut self, k: Keyboard, depressed: u32) {
            self.s
                .host([wl_keyboard::evt::modifiers(k.host, 1, depressed, 0, 0, 0)])
                .await;
            assert_eq!(self.s.guest_received(), [modifiers(k, 1, depressed)]);
        }

        async fn modifiers(&mut self, depressed: u32) {
            self.modifiers_on(self.k, depressed).await;
        }

        /// `k`'s host sends the default keymap again, which the guest gets.
        async fn keymap(&mut self) {
            let k = Object {
                id: self.k.id,
                host: self.k.host,
            };
            self.s.keymap(k).await;
        }

        /// A guest text input of the seat, which the guest enables on the
        /// focused surface and the host activates.
        async fn text_input(&mut self) -> Ime {
            use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1::Request as ExtManager;
            use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1::Request as Manager;
            let manager = self
                .s
                .bind(
                    "zwp_text_input_manager_v1",
                    zwp_text_input_manager_v1::VERSION,
                )
                .await;
            self.s
                .advertise(
                    "zcr_text_input_extension_v1",
                    zcr_text_input_extension_v1::VERSION,
                )
                .await;
            // The proxy binds the extension for itself.
            self.s.host_received();
            let id = self.s.new_id();
            self.s
                .guest([zwp_text_input_manager_v3::req::get_text_input(
                    manager, id, self.seat,
                )])
                .await;
            let host = self.s.host_received();
            let [(_, AnyRequest::ZwpTextInputManagerV1(Manager::CreateTextInput { id: host_id })), (
                _,
                AnyRequest::ZcrTextInputExtensionV1(ExtManager::GetExtendedTextInput {
                    id: ext,
                    ..
                }),
            )] = host[..]
            else {
                panic!("the host got no text input: {host:?}");
            };
            let surface = self.surface.id;
            let enter = zwp_text_input_v3::Event::Enter { surface };
            assert_eq!(self.s.guest_received(), [(id, enter.into())]);
            self.s
                .guest([
                    zwp_text_input_v3::req::enable(id),
                    zwp_text_input_v3::req::commit(id),
                ])
                .await;
            // The activation, which the text-input tests pin.
            let host_seat = self
                .s
                .host_received()
                .into_iter()
                .find_map(|request| match request {
                    (
                        sender,
                        AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Activate {
                            seat,
                            ..
                        }),
                    ) if sender == host_id => Some(seat),
                    _ => None,
                })
                .expect("the host activates the text input");
            Ime {
                id,
                host: host_id,
                ext,
                host_seat,
            }
        }

        /// The host input method confirms its empty composition.
        async fn confirm(&mut self, ime: Ime) {
            self.s
                .host([zcr_extended_text_input_v1::evt::confirm_preedit(ime.ext, 0)])
                .await;
        }

        /// The host input method sends a keysym, which Exo stamps with the
        /// timestamp `time` and the Wayland serial `serial` in swapped
        /// fields.
        async fn keysym(&mut self, ime: Ime, time: u32, serial: u32, sym: u32, state: u32) {
            self.s
                .host([zwp_text_input_v1::evt::keysym(
                    ime.host, time, serial, sym, state, 0,
                )])
                .await;
        }
    }

    /// The host `zcr_extended_keyboard_v1` the proxy creates for `k`, from
    /// the requests the host got.
    fn extended_keyboard_of(k: Keyboard, host: &[(u32, AnyRequest)]) -> u32 {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::Request;
        let [(_, AnyRequest::ZcrKeyboardExtensionV1(Request::GetExtendedKeyboard { id, keyboard }))] =
            host[..]
        else {
            panic!("the host got no extended keyboard: {host:?}");
        };
        assert_eq!(keyboard, k.host);
        id
    }

    fn entered(k: Keyboard, serial: u32, surface: u32, keys: &[u32]) -> (u32, AnyEvent) {
        let keys = keys.iter().flat_map(|key| key.to_ne_bytes()).collect();
        let enter = wl_keyboard::Event::Enter {
            serial,
            surface,
            keys,
        };
        (k.id, enter.into())
    }

    fn left(k: Keyboard, serial: u32, surface: u32) -> (u32, AnyEvent) {
        (k.id, wl_keyboard::Event::Leave { serial, surface }.into())
    }

    fn modifiers(k: Keyboard, serial: u32, depressed: u32) -> (u32, AnyEvent) {
        let modifiers = wl_keyboard::Event::Modifiers {
            serial,
            mods_depressed: depressed,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        (k.id, modifiers.into())
    }

    fn key_event(k: Keyboard, serial: u32, time: u32, key: u32, state: u32) -> (u32, AnyEvent) {
        let event = wl_keyboard::Event::Key {
            serial,
            time,
            key,
            state,
        };
        (k.id, event.into())
    }

    /// The press and release that IME recovery replays for `key` at
    /// `time`, with synthetic serials `serial` and `serial + 1`.
    fn recovered(k: Keyboard, serial: u32, time: u32, key: u32) -> [(u32, AnyEvent); 2] {
        [
            key_event(k, serial, time, key, PRESSED),
            key_event(k, serial + 1, time, key, RELEASED),
        ]
    }

    fn acked(k: Keyboard, serial: u32, handled: u32) -> (u32, AnyRequest) {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::Request;
        (k.ext, Request::AckKey { serial, handled }.into())
    }

    /// What the host gets when the guest releases `k`.
    fn released(k: Keyboard) -> [(u32, AnyRequest); 2] {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
        [
            (k.host, wl_keyboard::Request::Release {}.into()),
            (k.ext, zcr_extended_keyboard_v1::Request::Destroy {}.into()),
        ]
    }

    /// Whether `host` is what the host gets when the proxy deactivates the
    /// host text input of `ime`: the deactivate, and the sync that drains
    /// the events the host sent before it.
    fn deactivated(ime: Ime, host: &[(u32, AnyRequest)]) -> bool {
        let deactivate = (
            ime.host,
            zwp_text_input_v1::Request::Deactivate {
                seat: ime.host_seat,
            }
            .into(),
        );
        matches!(
            host,
            [first, (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { .. }))]
                if *first == deactivate
        )
    }

    /// The guest's preedit `text`, with the cursor at its end.
    fn v3_preedit(ime: Ime, text: &str) -> (u32, AnyEvent) {
        let end = text.len() as i32;
        let preedit = zwp_text_input_v3::Event::PreeditString {
            text: Some(text.into()),
            cursor_begin: end,
            cursor_end: end,
        };
        (ime.id, preedit.into())
    }

    fn v3_commit(ime: Ime, text: &str) -> (u32, AnyEvent) {
        let commit = zwp_text_input_v3::Event::CommitString {
            text: Some(text.into()),
        };
        (ime.id, commit.into())
    }

    fn v3_enter(ime: Ime, surface: u32) -> (u32, AnyEvent) {
        (ime.id, zwp_text_input_v3::Event::Enter { surface }.into())
    }

    fn v3_leave(ime: Ime, surface: u32) -> (u32, AnyEvent) {
        (ime.id, zwp_text_input_v3::Event::Leave { surface }.into())
    }

    /// The text-input `done` that closes the guest's first commit.
    fn done(ime: Ime) -> (u32, AnyEvent) {
        (ime.id, zwp_text_input_v3::Event::Done { serial: 1 }.into())
    }

    // -----------------------------------------------------------------------
    // Chords
    // -----------------------------------------------------------------------

    #[test]
    fn chord_classification_follows_the_configuration() {
        use WindowLayoutAction::*;
        let arc = |spec: &str| Arc::new(PlacementConfig::arc_bounds_for_test(spec));
        let resolve = |variables: &[(&str, &str)]| {
            Arc::new(PlacementConfig::resolve_for_test(
                variables,
                None,
                ArcIdAllocator::block_for_test(),
            ))
        };
        let nine = arc(
            "<Alt>q=top-left,<Alt>w=top,<Alt>e=top-right,<Alt>a=left,<Alt>s=fullscreen,\
             <Alt>d=right,<Alt>z=bottom-left,<Alt>x=bottom,<Alt>c=bottom-right",
        );
        let alt_q = arc("<Alt>q=top-left");
        let control_q = arc("<Control>q=top-left");
        // The ARC bounds policy is off, so the binding is ignored.
        let ungated = resolve(&[("SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS", "<Alt>q=top-left")]);
        let both = resolve(&[
            ("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1"),
            ("SOMMELIER_ACCELERATORS", "<Control>a"),
            (
                "SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS",
                "<Control>q=top-left",
            ),
        ]);
        let cases = [
            (&nine, ALT, KEY_Q, KeyClass::Placement(TopLeft)),
            (&nine, ALT, KEY_W, KeyClass::Placement(Top)),
            (&nine, ALT, KEY_E, KeyClass::Placement(TopRight)),
            (&nine, ALT, KEY_A, KeyClass::Placement(Left)),
            (&nine, ALT, KEY_S, KeyClass::Placement(Fullscreen)),
            (&nine, ALT, KEY_D, KeyClass::Placement(Right)),
            (&nine, ALT, KEY_Z, KeyClass::Placement(BottomLeft)),
            (&nine, ALT, KEY_X, KeyClass::Placement(Bottom)),
            (&nine, ALT, KEY_C, KeyClass::Placement(BottomRight)),
            // A binding needs exactly its modifiers.
            (&alt_q, 0, KEY_Q, KeyClass::Guest),
            (&alt_q, ALT | SHIFT, KEY_Q, KeyClass::Guest),
            (&control_q, CONTROL, KEY_Q, KeyClass::Placement(TopLeft)),
            (&ungated, ALT, KEY_Q, KeyClass::Guest),
            (&both, CONTROL, KEY_A, KeyClass::HostAccelerator),
            (&both, CONTROL, KEY_Q, KeyClass::Placement(TopLeft)),
            (&both, CONTROL, KEY_Z, KeyClass::Guest),
        ];
        for (placement, modifiers, key, class) in cases {
            let mut p = Proxy::with(Context::with_placement_for_test(Arc::clone(placement)));
            p.keymap(KEYBOARD);
            p.modifiers(KEYBOARD, modifiers);
            assert_eq!(
                p.classify(KEYBOARD, key),
                class,
                "key {key} with modifiers {modifiers:#x}"
            );
        }
    }

    #[test]
    fn a_key_forms_no_chord_without_a_keymap_or_beyond_the_keycode_range() {
        let mut p = Proxy::new();
        let keyboard = HostId(KEYBOARD.host);
        assert_eq!(
            KeyboardHandler::current_chord(&p.ctx, keyboard, KEY_A),
            None
        );
        p.keymap(KEYBOARD);
        // XKB's keycode is the evdev keycode plus 8.
        assert_eq!(
            KeyboardHandler::current_chord(&p.ctx, keyboard, u32::MAX),
            None
        );
    }

    #[test]
    fn compositor_shortcut_is_handled_and_release_is_suppressed() {
        let keyboard = HostId(5);
        let key = 24;
        let mut ctx = Context::new_for_test(false, false, vec![]);

        let press = ctx.transition_guest_key(keyboard, key, GuestKeyEvent::CompositorShortcutPress);
        assert_eq!(press.delivery, GuestKeyDelivery::Drop);
        assert_eq!(press.ack_handled, Some(true));
        assert_eq!(ctx.guest_key_owner(keyboard, key), Some(CompositorShortcut));

        let release =
            ctx.transition_guest_key(keyboard, key, GuestKeyEvent::CompositorShortcutRelease);
        assert_eq!(release.delivery, GuestKeyDelivery::Drop);
        assert_eq!(release.ack_handled, Some(true));
        assert!(release.ends_repeat);
        assert_eq!(ctx.guest_key_owner(keyboard, key), None);
    }

    // -----------------------------------------------------------------------
    // Placement shortcuts
    // -----------------------------------------------------------------------

    /// The keyboard of the placement tests.
    const PLACING: Mapped = Mapped {
        guest: 15,
        host: 5,
        extended: 50,
        seat: 11,
    };

    /// The text input the placement tests compose in, on the placed surface.
    const PLACED_TEXT_INPUT: MappedTextInput = MappedTextInput {
        guest: 43,
        host: 143,
        extended: 60,
    };

    /// The placed window: xdg_toplevel 13 (host 23) of wl_surface 12 (host
    /// 22), on whose output 25 Aura shell 24 places it.
    const PLACED_TOPLEVEL: u32 = 13;
    const PLACED_SURFACE: u32 = 12;
    const PLACED_HOST_SURFACE: u32 = 22;

    /// A 3840x2160 output whose Aura scales are known.
    fn placeable_output() -> OutputState {
        OutputState {
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        }
    }

    /// `PLACING` focused on the placed window on `output`, with `placement`
    /// configured and Alt held.
    fn placeable(placement: impl Into<Arc<PlacementConfig>>, output: OutputState) -> Proxy {
        let mut p = Proxy::with(Context::with_placement_for_test(placement));
        p.map(PLACING);
        p.surface(PLACED_SURFACE, PLACED_HOST_SURFACE);
        p.ctx
            .shadow_table
            .insert_paired_for_test(PLACED_TOPLEVEL, 23, "xdg_toplevel");
        p.ctx
            .xdg_roles
            .add_toplevel_role_for_test(1013, PLACED_SURFACE, PLACED_TOPLEVEL);
        p.focus(PLACING, PLACED_SURFACE);
        p.ctx.bind_aura_shell_for_test(24);
        p.ctx.outputs.bind_for_test(25, output);
        p.keymap(PLACING);
        p.modifiers(PLACING, ALT);
        p
    }

    /// [`placeable`] with `<Alt>q=top-left` configured.
    fn placeable_shortcut() -> Proxy {
        placeable(
            PlacementConfig::arc_bounds_for_test("<Alt>q=top-left"),
            placeable_output(),
        )
    }

    /// A placement configuration whose bindings come from a file.
    fn file_placement(dir: &crate::test_support::TempDir, document: &[u8]) -> Arc<PlacementConfig> {
        let path = dir.write("shortcuts.toml", document);
        Arc::new(PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1")],
            Some(&path),
            ArcIdAllocator::block_for_test(),
        ))
    }

    /// The `[x, y, width, height]` of each `set_window_bounds` queued for
    /// the placed window.
    fn queued_bounds(ctx: &Context) -> Vec<[i32; 4]> {
        let Some(aura_toplevel) = ctx
            .window_placement
            .aura_toplevel_for_xdg_toplevel(PLACED_TOPLEVEL)
        else {
            return Vec::new();
        };
        ctx.client_to_host_queue
            .iter()
            .filter(|(message, _)| msg_sender(message) == aura_toplevel)
            .filter_map(|(message, _)| {
                match decode(
                    message,
                    &mut Default::default(),
                    zaura_toplevel::Request::from_wire,
                ) {
                    Ok((
                        _,
                        zaura_toplevel::Request::SetWindowBounds {
                            x,
                            y,
                            width,
                            height,
                            ..
                        },
                    )) => Some([x, y, width, height]),
                    _ => None,
                }
            })
            .collect()
    }

    /// Where the `<Alt>q=top-left` placement puts the placed window.
    const TOP_LEFT_BOUNDS: [i32; 4] = [0, 0, 1920, 1080];

    /// The host completes internal callback `callback`.
    fn complete_callback(ctx: &mut Context, callback: u32) {
        use crate::protocols::wayland::wl_callback::WlCallbackHandler;
        ctx.last_sender_id = callback;
        assert_eq!(
            crate::handler::callback::CallbackHandler.on_done(ctx, 0),
            Action::Drop
        );
    }

    /// `PLACED_TEXT_INPUT`, composing a preedit on the placed surface.
    fn compose_on_placed_surface(p: &mut Proxy) {
        p.text_input(PLACED_TEXT_INPUT, PLACING.seat, PLACED_SURFACE);
        // Text-input activation requests name the host seat.
        p.ctx
            .shadow_table
            .insert_paired_for_test(PLACING.seat, 70, "wl_seat");
        p.ctx
            .text_inputs
            .get_mut(&PLACED_TEXT_INPUT.guest)
            .unwrap()
            .current_preedit = "한".to_string();
    }

    /// A matched chord belongs to Sommelier while the policy is active, so
    /// it is consumed even when placement has to be skipped. The skip leaves
    /// no Aura objects or barriers behind.
    #[test]
    fn placement_chord_is_consumed_while_aura_scale_metadata_is_incomplete() {
        // The device scale event has not arrived yet.
        let output = OutputState {
            aura_device_scale_factor: None,
            ..placeable_output()
        };
        let mut p = placeable(
            PlacementConfig::arc_bounds_for_test("<Alt>q=top-left"),
            output,
        );
        assert_eq!(p.classify(PLACING, KEY_Q), KeyClass::Placement(TopLeft));

        assert_eq!(
            p.key(PLACING, 1, 100, KEY_Q, PRESSED),
            Action::Drop,
            "a skipped placement must not leak the chord to the guest"
        );
        assert_eq!(p.owner(PLACING, KEY_Q), Some(CompositorShortcut));
        assert_eq!(p.key(PLACING, 2, 101, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
        assert_eq!(p.ctx.window_placement.aura_toplevel_count(), 0);
        assert_eq!(p.ctx.aura_surfaces.count(), 0);
        assert_eq!(p.ctx.internal_callbacks.count(), 0);
        assert!(
            p.ctx
                .client_to_host_queue
                .iter()
                .all(|(message, _)| msg_sender(message) == PLACING.extended),
            "only keyboard acknowledgments may be queued when placement lacks geometry"
        );
    }

    #[test]
    fn placement_is_skipped_rather_than_moved_to_another_output() {
        let mut p = placeable_shortcut();
        let entered_output = 26;
        p.ctx.outputs.bind_for_test(
            entered_output,
            OutputState {
                origin_x: 3840,
                mode_width: 1920,
                mode_height: 1080,
                aura_current_scale: Some(1000),
                // The device scale event for this output has not arrived.
                ..Default::default()
            },
        );
        p.ctx
            .outputs
            .surface_entered(PLACED_HOST_SURFACE, entered_output);
        assert!(
            p.ctx.outputs.primary().is_some(),
            "another output is usable, but the surface is not on it"
        );

        assert_eq!(
            place_focused(&mut p.ctx, HostId(PLACING.host), TopLeft),
            Err(PlacementSkip::NoUsableOutput)
        );
        assert_eq!(p.key(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert!(queued_bounds(&p.ctx).is_empty());
    }

    /// The press, repeat and release of a matched chord stay with Sommelier
    /// even when its placement is skipped, for every reason to skip it.
    #[test]
    fn a_skipped_placement_still_consumes_its_chord() {
        let dir = crate::test_support::TempDir::new("keyboard-sliver");
        // 0.0001 of the 3840-pixel work area floors onto the left border.
        let sliver = WindowLayoutAction::Rect(
            crate::window_layout::NormalizedRect::new(0.0001, 0.0, 0.9999, 1.0).unwrap(),
        );
        let mut no_toplevel = placeable_shortcut();
        // The focused surface is not (or no longer) an xdg_toplevel.
        no_toplevel.ctx.xdg_roles = crate::state::XdgRoles::default();
        let sliver_binding =
            b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\nrect = [0.0001, 0.0, 0.9999, 1.0]\n";
        let no_arc_identity = PlacementConfig::resolve_for_test(
            &[
                ("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1"),
                ("SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS", "<Alt>q=top-left"),
            ],
            None,
            ArcIdAllocator::exhausted_for_test(),
        );
        let cases = [
            (no_toplevel, TopLeft, PlacementSkip::NoFocusedToplevel),
            (
                placeable(file_placement(&dir, sliver_binding), placeable_output()),
                sliver,
                PlacementSkip::RectNotRepresentable,
            ),
            (
                placeable(no_arc_identity, placeable_output()),
                TopLeft,
                PlacementSkip::NoAuraToplevel,
            ),
        ];
        for (mut p, action, skip) in cases {
            assert_eq!(
                place_focused(&mut p.ctx, HostId(PLACING.host), action),
                Err(skip)
            );
            assert_eq!(
                p.key(PLACING, 2, 100, KEY_Q, PRESSED),
                Action::Drop,
                "{skip:?}"
            );
            assert!(queued_bounds(&p.ctx).is_empty(), "{skip:?}");
            assert_eq!(
                p.owner(PLACING, KEY_Q),
                Some(CompositorShortcut),
                "{skip:?}"
            );
            assert_eq!(
                p.key(PLACING, 2, 130, KEY_Q, REPEATED),
                Action::Drop,
                "{skip:?}"
            );
            assert_eq!(
                p.key(PLACING, 3, 160, KEY_Q, RELEASED),
                Action::Drop,
                "{skip:?}"
            );
            assert_eq!(p.owner(PLACING, KEY_Q), None, "{skip:?}");
            assert_eq!(p.ctx.internal_callbacks.count(), 0, "{skip:?}");
            assert_eq!(p.ctx.window_placement.aura_toplevel_count(), 0, "{skip:?}");
        }
        assert_eq!(
            PlacementSkip::NoFocusedToplevel.log_level(),
            log::Level::Debug,
            "a transient skip stays at debug"
        );
        assert_eq!(
            PlacementSkip::RectNotRepresentable.log_level(),
            log::Level::Info,
            "a persistent misconfiguration is visible at info"
        );
    }

    #[test]
    fn shortcut_config_reload_takes_effect_on_the_next_press() {
        let dir = crate::test_support::TempDir::new("keyboard-shortcut-reload");
        let placement = file_placement(&dir, b"version = 1\n");
        let mut p = placeable(Arc::clone(&placement), placeable_output());
        assert_eq!(
            p.classify(PLACING, KEY_Q),
            KeyClass::Guest,
            "the file is the only binding source"
        );

        dir.write(
            "shortcuts.toml",
            b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\nrect = [0.25, 0.0, 0.5, 1.0]\n",
        );
        placement.reload().unwrap().unwrap();
        assert_eq!(
            p.key(PLACING, 1, 100, KEY_Q, PRESSED),
            Action::Drop,
            "the reloaded chord is consumed by the compositor"
        );
        assert_eq!(queued_bounds(&p.ctx), [[960, 0, 1920, 2160]]);
        assert_eq!(p.owner(PLACING, KEY_Q), Some(CompositorShortcut));

        // Removing the binding while the key is held still suppresses the
        // matching release: ownership, not the binding, owns the release.
        dir.write("shortcuts.toml", b"version = 1\n");
        placement.reload().unwrap().unwrap();
        assert_eq!(p.key(PLACING, 2, 101, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
        p.ctx.client_to_host_queue.clear();
        assert_eq!(
            p.key(PLACING, 3, 102, KEY_Q, PRESSED),
            Action::Forward,
            "a removed binding is delivered to the guest"
        );
        assert!(queued_bounds(&p.ctx).is_empty());
    }

    /// Contract: whoever delivers a key generation to the guest first owns
    /// it, and placement never acts on a key the guest already received.
    ///
    /// Text-input may deliver the keysym before any `peek_key` for the
    /// generation could classify it as a placement chord, or IME recovery
    /// may already have replayed it as a balanced pair, which only an
    /// eligible peek generation can be and a peeked placement chord never
    /// is. Placing the window as well would make one key do two things.
    /// With a preceding `peek_key`, a placement chord never reaches the
    /// guest this way: see
    /// `peeked_placement_chord_is_not_claimed_by_text_input_or_ime_recovery`.
    #[test]
    fn a_chord_the_guest_already_received_stays_the_guests() {
        let cases = [
            (
                GuestKeyEvent::TextInputPress { serial: 1 },
                GuestKeyDelivery::Forward,
                TextInputKeysym,
                // It closes the press text-input delivered.
                Action::Forward,
            ),
            (
                GuestKeyEvent::RecoverIme,
                GuestKeyDelivery::EmitBalancedPair,
                ImeRecovery,
                // The recovered pair is already balanced.
                Action::Drop,
            ),
        ];
        for (event, delivery, owner, release) in cases {
            let mut p = placeable_shortcut();
            let keyboard = HostId(PLACING.host);
            let decision = p.ctx.transition_guest_key(keyboard, KEY_Q, event);
            assert_eq!(decision.delivery, delivery, "{event:?}");
            assert_eq!(
                p.key(PLACING, 2, 100, KEY_Q, PRESSED),
                Action::Drop,
                "{event:?}: the physical channel must not duplicate the guest press"
            );
            assert_eq!(p.owner(PLACING, KEY_Q), Some(owner), "{event:?}");
            assert_eq!(
                p.key(PLACING, 3, 101, KEY_Q, RELEASED),
                release,
                "{event:?}"
            );
            assert_eq!(p.ctx.window_placement.aura_toplevel_count(), 0, "{event:?}");
            assert_eq!(p.ctx.internal_callbacks.count(), 0, "{event:?}");
            assert!(queued_bounds(&p.ctx).is_empty(), "{event:?}");
        }
    }

    #[test]
    fn peeked_placement_chord_is_not_claimed_by_text_input_or_ime_recovery() {
        let mut p = placeable_shortcut();
        let keyboard = HostId(PLACING.host);

        // peek_key reports the physical press before the IME channel and the
        // wl_keyboard event for the same generation.
        assert_eq!(p.peek(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert!(
            p.ctx
                .key_generations
                .peek(keyboard, KEY_Q)
                .is_some_and(|press| !press.eligible),
            "a placement chord must never become an IME recovery candidate"
        );
        let text_input = GuestKeyEvent::TextInputPress { serial: 2 };
        assert_eq!(
            p.ctx
                .transition_guest_key(keyboard, KEY_Q, text_input)
                .delivery,
            GuestKeyDelivery::Drop,
            "text-input must not deliver a placement chord to the guest"
        );
        assert_eq!(p.owner(PLACING, KEY_Q), None);

        assert_eq!(
            p.key(PLACING, 2, 100, KEY_Q, PRESSED),
            Action::Drop,
            "the chord stays Sommelier's"
        );
        assert_eq!(
            queued_bounds(&p.ctx),
            [TOP_LEFT_BOUNDS],
            "the window is placed"
        );
        assert_eq!(p.owner(PLACING, KEY_Q), Some(CompositorShortcut));

        assert_eq!(p.peek(PLACING, 3, 101, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.key(PLACING, 3, 101, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
    }

    /// The host IME consumed the chord, so no `wl_keyboard.key` arrives.
    /// The chord is neither placed (placement runs only on
    /// `wl_keyboard.key`) nor delivered to the guest through text-input.
    #[test]
    fn placement_chord_consumed_by_the_host_ime_is_neither_placed_nor_delivered() {
        let mut p = placeable_shortcut();
        compose_on_placed_surface(&mut p);
        let guest_messages = p.ctx.host_to_client_queue.len();

        let t = PLACED_TEXT_INPUT;
        assert_eq!(p.peek(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert_eq!(p.keysym(t, 100, 2, keysyms::KEY_q, PRESSED), Action::Drop);
        assert_eq!(p.keysym(t, 101, 3, keysyms::KEY_q, RELEASED), Action::Drop);
        assert_eq!(p.peek(PLACING, 3, 101, KEY_Q, RELEASED), Action::Drop);

        assert_eq!(
            p.ctx.host_to_client_queue.len(),
            guest_messages,
            "nothing reaches the guest"
        );
        assert_eq!(p.owner(PLACING, KEY_Q), None);
        assert!(queued_bounds(&p.ctx).is_empty());
        assert_eq!(p.ctx.window_placement.aura_toplevel_count(), 0);
    }

    #[test]
    fn duplicate_shortcut_press_does_not_queue_another_placement() {
        let mut p = placeable_shortcut();
        assert_eq!(p.key(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert_eq!(queued_bounds(&p.ctx), [TOP_LEFT_BOUNDS]);
        let barriers = p.ctx.internal_callbacks.count();

        assert_eq!(
            p.key(PLACING, 2, 130, KEY_Q, PRESSED),
            Action::Drop,
            "a duplicate press of a consumed shortcut stays consumed"
        );
        assert_eq!(
            queued_bounds(&p.ctx),
            [TOP_LEFT_BOUNDS],
            "a duplicate press must not place the window again"
        );
        assert_eq!(p.ctx.internal_callbacks.count(), barriers);
        assert_eq!(p.owner(PLACING, KEY_Q), Some(CompositorShortcut));

        assert_eq!(p.key(PLACING, 3, 160, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
    }

    #[test]
    fn peek_after_consumed_shortcut_press_is_not_an_ime_recovery_candidate() {
        let mut p = placeable_shortcut();
        let keyboard = HostId(PLACING.host);
        assert_eq!(p.key(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert_eq!(queued_bounds(&p.ctx), [TOP_LEFT_BOUNDS]);

        // The extension's peek for the same generation trails the
        // wl_keyboard press.
        assert_eq!(p.peek(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert!(
            p.ctx
                .key_generations
                .peek(keyboard, KEY_Q)
                .is_some_and(|press| !press.eligible),
            "a consumed shortcut generation must not be recoverable"
        );
        assert_eq!(
            p.ctx
                .transition_guest_key(keyboard, KEY_Q, GuestKeyEvent::RecoverIme)
                .delivery,
            GuestKeyDelivery::Drop,
            "an empty IME confirmation must not replay the consumed key"
        );

        assert_eq!(p.peek(PLACING, 3, 101, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.key(PLACING, 3, 101, KEY_Q, RELEASED), Action::Drop);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
    }

    #[test]
    fn placement_shortcuts_leave_the_focused_ime_generation_untouched() {
        // The old generation-safe stack (#8) reset, deactivated, and replayed
        // the host text-input generation around each shortcut because its
        // transient ARC identity and self-parent paths changed Aura metadata
        // per placement. The bounds-only design installs the ARC identity at
        // most once per surface, so repeated placements must not touch the
        // focused IME generation, including an in-progress composition.
        let mut p = placeable_shortcut();
        compose_on_placed_surface(&mut p);
        let t = PLACED_TEXT_INPUT;
        let before = p.ctx.text_inputs[&t.guest].clone();

        for (serial, time) in [(2, 100), (4, 200)] {
            assert_eq!(p.key(PLACING, serial, time, KEY_Q, PRESSED), Action::Drop);
            assert_eq!(
                p.key(PLACING, serial + 1, time + 1, KEY_Q, RELEASED),
                Action::Drop
            );
            let aura_toplevel = p
                .ctx
                .window_placement
                .aura_toplevel_for_xdg_toplevel(PLACED_TOPLEVEL)
                .expect("placement must create the Aura toplevel");
            let barrier = p
                .ctx
                .window_placement
                .active_barrier_for_toplevel(aura_toplevel)
                .expect("placement must order its bounds behind a barrier");
            complete_callback(&mut p.ctx, barrier);
            assert!(!p.ctx.window_placement.has_active_barrier(aura_toplevel));
        }
        assert_eq!(queued_bounds(&p.ctx), [TOP_LEFT_BOUNDS; 2]);

        let aura_surface = p
            .ctx
            .aura_surfaces
            .get(PLACED_HOST_SURFACE)
            .expect("placement must create the Aura surface");
        let identities = p.ctx.client_to_host_queue.iter().filter(|(message, _)| {
            msg_sender(message) == aura_surface
                && msg_opcode(message) == zaura_surface::REQ_SET_APPLICATION_ID
        });
        assert_eq!(
            identities.count(),
            1,
            "the ARC identity is installed once, not changed per placement"
        );
        assert!(
            p.ctx
                .client_to_host_queue
                .iter()
                .all(|(message, _)| ![t.host, t.extended].contains(&msg_sender(message))),
            "placement must not reset, deactivate, or replay host text input"
        );
        assert!(
            p.ctx
                .host_to_client_queue
                .iter()
                .all(|(message, _)| msg_sender(message) != t.guest),
            "placement must not synthesize guest text-input focus or editor events"
        );
        assert_eq!(p.ctx.text_inputs[&t.guest], before);
        assert_eq!(
            p.ctx.keyboard_focus.surface_for_seat(PLACING.seat),
            Some(PLACED_SURFACE)
        );
    }

    #[test]
    fn focus_leave_during_consumed_shortcut_does_not_swallow_the_next_press() {
        let mut p = placeable_shortcut();
        assert_eq!(p.key(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        assert_eq!(p.owner(PLACING, KEY_Q), Some(CompositorShortcut));

        // Focus leaves before the chord's release, which the old surface
        // will never receive. The consumed generation must not survive and
        // swallow the first press after focus returns.
        assert_eq!(p.leave(PLACING, PLACED_HOST_SURFACE), Action::Forward);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
        // Exo sends the released Alt before the enter.
        p.modifiers(PLACING, 0);
        assert_eq!(p.enter(PLACING, PLACED_HOST_SURFACE, &[]), Action::Forward);

        assert_eq!(
            p.key(PLACING, 6, 300, KEY_Q, PRESSED),
            Action::Forward,
            "a plain press after focus returns is ordinary guest input"
        );
        assert_eq!(p.owner(PLACING, KEY_Q), Some(Physical));
        assert_eq!(p.key(PLACING, 7, 301, KEY_Q, RELEASED), Action::Forward);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
        assert_eq!(queued_bounds(&p.ctx), [TOP_LEFT_BOUNDS]);
    }

    #[test]
    fn chord_key_held_across_refocus_is_released_to_the_guest_that_saw_it() {
        let mut p = placeable_shortcut();
        assert_eq!(p.key(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);

        // The host re-enters with the chord key still held. The forwarded
        // enter's keys array reports it pressed to the guest, so its later
        // release belongs to the guest rather than to the consumed shortcut.
        assert_eq!(p.leave(PLACING, PLACED_HOST_SURFACE), Action::Forward);
        assert_eq!(
            p.enter(PLACING, PLACED_HOST_SURFACE, &[KEY_Q]),
            Action::Forward
        );
        assert_eq!(p.owner(PLACING, KEY_Q), Some(Physical));
        assert_eq!(p.key(PLACING, 5, 101, KEY_Q, RELEASED), Action::Forward);
        assert_eq!(p.owner(PLACING, KEY_Q), None);
        assert_eq!(queued_bounds(&p.ctx), [TOP_LEFT_BOUNDS]);
    }

    #[test]
    fn ime_focus_churn_inside_a_placement_barrier_keeps_both_barriers_separate() {
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::ZwpTextInputV3Handler;

        let mut p = placeable_shortcut();
        compose_on_placed_surface(&mut p);
        let t = PLACED_TEXT_INPUT;
        assert_eq!(p.key(PLACING, 2, 100, KEY_Q, PRESSED), Action::Drop);
        let aura_toplevel = p
            .ctx
            .window_placement
            .aura_toplevel_for_xdg_toplevel(PLACED_TOPLEVEL)
            .expect("placement must create the Aura toplevel");
        let placement_barrier = p
            .ctx
            .window_placement
            .active_barrier_for_toplevel(aura_toplevel)
            .expect("placement must order its bounds behind a barrier");

        // A real host focus cycle lands while the placement barrier holds
        // configures. It drains the host IME generation through its own
        // text-input barrier and the guest re-enables on the new enter.
        assert_eq!(p.leave(PLACING, PLACED_HOST_SURFACE), Action::Forward);
        let ime_barrier = p.ctx.text_inputs[&t.guest]
            .draining_callback()
            .expect("focus leave must drain the host IME generation");
        assert_ne!(ime_barrier.0, placement_barrier);
        assert!(p.ctx.text_inputs[&t.guest].current_preedit.is_empty());
        assert_eq!(p.enter(PLACING, PLACED_HOST_SURFACE, &[]), Action::Forward);
        p.ctx.last_sender_id = t.guest;
        let mut v3 = crate::handler::text_input::TextInputV3Handler;
        assert_eq!(v3.on_enable(&mut p.ctx), Action::Drop);
        assert_eq!(v3.on_commit(&mut p.ctx), Action::Drop);
        assert!(!p.ctx.text_inputs[&t.guest].host_is_active());

        // The IME barrier reactivates text input without retiring the
        // placement barrier, which still gates configures.
        let queued_before = p.ctx.client_to_host_queue.len();
        complete_callback(&mut p.ctx, ime_barrier.0);
        assert!(p.ctx.text_inputs[&t.guest].host_is_active());
        // The activation, then the committed content type it replays.
        let (activate, _) = &p.ctx.client_to_host_queue[queued_before];
        assert_eq!(
            decode(
                activate,
                &mut Default::default(),
                zwp_text_input_v1::Request::from_wire
            ),
            Ok((
                t.host,
                zwp_text_input_v1::Request::Activate {
                    seat: 70,
                    surface: PLACED_HOST_SURFACE
                }
            ))
        );
        assert!(p.ctx.window_placement.has_active_barrier(aura_toplevel));

        // The placement barrier then retires without disturbing the fresh
        // IME generation.
        let host_queue_len = p.ctx.client_to_host_queue.len();
        let state_after_activation = p.ctx.text_inputs[&t.guest].clone();
        complete_callback(&mut p.ctx, placement_barrier);
        assert!(!p.ctx.window_placement.has_active_barrier(aura_toplevel));
        assert_eq!(p.ctx.client_to_host_queue.len(), host_queue_len);
        assert_eq!(p.ctx.text_inputs[&t.guest], state_after_activation);
    }

    // -----------------------------------------------------------------------
    // Keymaps
    // -----------------------------------------------------------------------

    /// The proxy maps a keymap, rather than reading it, so it loads from a
    /// memfd whose offset the host left at its end. It compiles to the
    /// keycodes and modifier masks the tests name.
    #[test]
    fn keymap_loads_from_non_rewound_memfd() {
        let mut p = Proxy::new();
        p.keymap(KEYBOARD);

        let keyboard = HostId(KEYBOARD.host);
        assert!(p.ctx.keyboard_xkb_states.contains_key(&keyboard));
        let keycodes = &p.ctx.keyboard_keysym_to_keycode[&keyboard];
        for (sym, key) in [
            (keysyms::KEY_q, KEY_Q),
            (keysyms::KEY_w, KEY_W),
            (keysyms::KEY_e, KEY_E),
            (keysyms::KEY_t, KEY_T),
            (keysyms::KEY_a, KEY_A),
            // A shifted keysym maps to its physical key.
            (keysyms::KEY_A, KEY_A),
            (keysyms::KEY_s, KEY_S),
            (keysyms::KEY_d, KEY_D),
            (keysyms::KEY_Shift_L, KEY_LEFTSHIFT),
            (keysyms::KEY_z, KEY_Z),
            (keysyms::KEY_x, KEY_X),
            (keysyms::KEY_c, KEY_C),
            (keysyms::KEY_b, KEY_B),
            (keysyms::KEY_space, KEY_SPACE),
            (keysyms::KEY_BackSpace, BACKSPACE),
        ] {
            assert_eq!(keycodes.get(&sym), Some(&key), "keysym {sym:#x}");
        }
        let repeatable = &p.ctx.keyboard_repeatable_keys[&keyboard];
        assert!(repeatable.contains(&KEY_SPACE) && repeatable.contains(&BACKSPACE));
        assert!(!repeatable.contains(&KEY_LEFTSHIFT));
        let keymap = &p.ctx.keyboard_keymaps[&keyboard];
        let mask = |name| 1 << keymap.mod_get_index(name);
        assert_eq!(
            [SHIFT, CAPS_LOCK, CONTROL, ALT],
            [
                mask(xkb::MOD_NAME_SHIFT),
                mask(xkb::MOD_NAME_CAPS),
                mask(xkb::MOD_NAME_CTRL),
                mask(xkb::MOD_NAME_ALT)
            ]
        );
    }

    #[test]
    fn keymap_mmap_is_private_not_shared() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let fd = memfd_create(
            CString::new("test-keymap-private").unwrap().as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create failed");
        nix::unistd::ftruncate(&fd, 4096).expect("ftruncate failed");
        let view = MmapView::from_fd(fd.as_fd(), 4096).expect("mmap failed");
        let address = view.ptr.as_ptr() as usize;
        let smaps = std::fs::read_to_string("/proc/self/smaps").expect("read smaps");
        let mapping_header = smaps
            .lines()
            .position(|line| {
                let mut fields = line.split_whitespace();
                let Some(range) = fields.next() else {
                    return false;
                };
                let Some((start, end)) = range.split_once('-') else {
                    return false;
                };
                usize::from_str_radix(start, 16).ok().is_some_and(|start| {
                    usize::from_str_radix(end, 16)
                        .ok()
                        .is_some_and(|end| address >= start && address < end)
                })
            })
            .expect("mapping must appear in smaps");
        let vm_flags = smaps
            .lines()
            .skip(mapping_header + 1)
            .take_while(|line| {
                let Some(range) = line.split_whitespace().next() else {
                    return true;
                };
                let Some((start, end)) = range.split_once('-') else {
                    return true;
                };
                !(usize::from_str_radix(start, 16).is_ok()
                    && usize::from_str_radix(end, 16).is_ok())
            })
            .find_map(|line| line.strip_prefix("VmFlags:"))
            .expect("mapping must have VmFlags");
        assert!(
            !vm_flags.split_whitespace().any(|flag| flag == "sh"),
            "keymap must use MAP_PRIVATE, got shared mapping flags: {vm_flags}"
        );
    }

    /// A keymap the proxy cannot use replaces the loaded one: it is
    /// forwarded, and the keyboard's XKB and key state go, so that no chord
    /// is classified until a usable keymap arrives. The keyboard's
    /// modifiers, which Exo does not send again, and another keyboard's
    /// keymap stay.
    #[test]
    fn an_unusable_keymap_clears_the_loaded_one_and_is_forwarded() {
        let xkb_v1 = KEYMAP_FORMAT_XKB_V1;
        let not_xkb = b"this is not a valid xkb keymap\0";
        // A keymap only its format makes unusable.
        let usable = crate::proxy::harness::default_keymap();
        let cases: [(&str, u32, &[u8], u32); 8] = [
            ("no keymap", KEYMAP_FORMAT_NO_KEYMAP, b"", 100),
            ("an unsupported format", 99, &usable, usable.len() as u32),
            // mmap(len=0) is undefined.
            ("an empty keymap", xkb_v1, b"", 0),
            // The size is refused before the descriptor is mapped.
            (
                "a keymap over the size limit",
                xkb_v1,
                b"",
                MAX_KEYMAP_SIZE + 1,
            ),
            // Mapping past the end of the file would raise SIGBUS.
            ("a size beyond the file", xkb_v1, b"\0", 2),
            ("no trailing NUL", xkb_v1, b"xkb_keymap", 10),
            ("invalid UTF-8", xkb_v1, &[0xFF, 0xFE, 0xFD, 0], 4),
            (
                "an invalid XKB keymap",
                xkb_v1,
                not_xkb,
                not_xkb.len() as u32,
            ),
        ];
        for (case, format, bytes, size) in cases {
            let mut p = Proxy::new();
            p.keymap(KEYBOARD);
            p.keymap(OTHER_KEYBOARD);
            p.modifiers(KEYBOARD, CONTROL);
            let keyboard = HostId(KEYBOARD.host);
            let keys = &mut p.ctx.key_generations;
            keys.suppress_host_accelerator(keyboard, KEY_A);
            keys.observe_peek_press(keyboard, BACKSPACE, 1, 123, true);
            keys.cancel_backspace_repeat(keyboard, BACKSPACE);
            assert!(p.ctx.claim_guest_key(keyboard, BACKSPACE, ImeRecovery));

            assert_eq!(
                p.keymap_of(KEYBOARD, format, bytes, size),
                Action::Forward,
                "{case}"
            );

            let ctx = &p.ctx;
            assert!(!ctx.keyboard_keymaps.contains_key(&keyboard), "{case}");
            assert!(!ctx.keyboard_xkb_states.contains_key(&keyboard), "{case}");
            assert!(
                !ctx.keyboard_keysym_to_keycode.contains_key(&keyboard),
                "{case}"
            );
            assert!(
                !ctx.keyboard_repeatable_keys.contains_key(&keyboard),
                "{case}"
            );
            assert!(!p.held(KEYBOARD, BACKSPACE), "{case}");
            assert!(!p.cancelled(KEYBOARD), "{case}");
            assert!(
                ctx.key_generations.peek(keyboard, BACKSPACE).is_none(),
                "{case}"
            );
            assert_eq!(p.owner(KEYBOARD, BACKSPACE), None, "{case}");
            assert!(
                !ctx.key_generations
                    .host_accelerator_suppressed(keyboard, KEY_A),
                "{case}"
            );
            assert_eq!(
                ctx.keyboard_modifiers[&keyboard].depressed, CONTROL,
                "{case}"
            );
            let other = HostId(OTHER_KEYBOARD.host);
            assert!(
                ctx.keyboard_keymaps.contains_key(&other)
                    && ctx.keyboard_xkb_states.contains_key(&other),
                "{case}: another keyboard's keymap stays"
            );
        }
    }

    #[test]
    fn on_keymap_ignores_a_keymap_over_the_size_limit() {
        for (size, accepted) in [(MAX_KEYMAP_SIZE, true), (MAX_KEYMAP_SIZE + 1, false)] {
            let mut p = Proxy::new();
            // A valid keymap, padded with blanks to `size` with its NUL.
            let mut bytes = crate::proxy::harness::default_keymap();
            bytes.pop();
            bytes.resize(size as usize - 1, b' ');
            bytes.push(0);

            assert_eq!(
                p.keymap_of(KEYBOARD, KEYMAP_FORMAT_XKB_V1, &bytes, size),
                Action::Forward
            );
            assert_eq!(
                p.ctx.keyboard_keymaps.contains_key(&HostId(KEYBOARD.host)),
                accepted,
                "a keymap of {size} bytes"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Acknowledgments and key generations
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn host_accelerators_stay_with_the_host_and_other_keys_reach_the_guest() {
        let mut kb = Kb::with_accelerators("<Control>a").await;
        kb.modifiers(CONTROL).await;

        // ChromiumOS acks an accelerator's press and its unmatched release
        // NOT_HANDLED and forwards neither.
        assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);
        assert_eq!(kb.key(3, KEY_A, RELEASED).await, Route::Host);
        assert_eq!(
            kb.key(4, KEY_A, PRESSED).await,
            Route::Host,
            "the next press is classified again"
        );
        assert_eq!(kb.key(5, KEY_B, PRESSED).await, Route::Guest);
        assert_eq!(kb.key(6, KEY_B, RELEASED).await, Route::Guest);
    }

    #[tokio::test]
    async fn each_key_generation_reaches_the_guest_once() {
        use Route::*;
        let cases: [(&str, &[(u32, Route)]); 4] = [
            ("an untracked release", &[(RELEASED, Host)]),
            (
                "a repeat without a press",
                &[(REPEATED, Host), (RELEASED, Host)],
            ),
            (
                "a duplicate press",
                &[(PRESSED, Guest), (PRESSED, Consumed), (RELEASED, Guest)],
            ),
            (
                "a held key",
                &[(PRESSED, Guest), (REPEATED, Guest), (RELEASED, Guest)],
            ),
        ];
        for (case, events) in cases {
            let mut kb = Kb::new().await;
            for (serial, &(state, route)) in (2..).zip(events) {
                assert_eq!(kb.key(serial, KEY_A, state).await, route, "{case}");
            }
        }
    }

    /// The proxy creates a keyboard's extended keyboard at its first enter
    /// and acks every key through it from then on, even once the host
    /// removed the extension's global, until the keyboard ends.
    #[tokio::test]
    async fn the_extended_keyboard_acks_keys_from_the_first_enter_until_the_keyboard_ends() {
        let accelerators = crate::accelerator::parse_accelerators("<Control>a").unwrap();
        let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;

        // Before its first enter, a key has no extended keyboard to ack it.
        let press = wl_keyboard::evt::key(kb.k.host, 1, 1, KEY_B, PRESSED);
        kb.s.host([press]).await;
        assert_eq!(
            kb.s.guest_received(),
            [key_event(kb.k, 1, 1, KEY_B, PRESSED)]
        );
        assert_eq!(kb.s.host_received(), []);

        let surface = kb.surface;
        kb.k = kb.first_enter(kb.k, surface).await;
        let other_surface = kb.s.surface().await;
        let enter = wl_keyboard::evt::enter(kb.k.host, 2, other_surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        assert_eq!(
            kb.s.guest_received(),
            [
                entered(kb.k, 2, other_surface.id, &[]),
                modifiers(kb.k, 2, 0)
            ]
        );
        assert_eq!(
            kb.s.host_received(),
            [],
            "one extended keyboard per keyboard"
        );

        // The manager's global goes, but its child lives on.
        let registry = kb.s.registry().await;
        kb.s.host([wl_registry::evt::global_remove(registry.host, kb.extension)])
            .await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), []);
        kb.modifiers(CONTROL).await;
        assert_eq!(kb.key(3, KEY_A, PRESSED).await, Route::Host);

        // Ending the keyboard ends its extended keyboard, its modifiers and
        // its suppressed accelerator, as every batch checks.
        kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), released(kb.k));
    }

    /// Releasing a keyboard forgets the key state it owns, and only that.
    #[test]
    fn on_release_destroys_extended_keyboard_and_cleans_map() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        let keyboard = HostId(KEYBOARD.host);
        let other_seat = 8;
        let keys = &mut p.ctx.key_generations;
        keys.record_latest_peek(KEYBOARD.seat, None, keyboard, 1);
        keys.record_latest_peek(KEYBOARD.seat, Some(UNCREATED_SURFACE), keyboard, 2);
        keys.record_latest_peek(
            other_seat,
            Some(UNCREATED_SURFACE),
            HostId(OTHER_KEYBOARD.host),
            3,
        );
        // A text-input press whose guest release is retired.
        assert!(keys.claim_text_input_owner(keyboard, KEY_A, 1));
        keys.observe_physical_state(keyboard, KEY_A, RELEASED);
        keys.observe_physical_state(keyboard, KEY_A, PRESSED);

        p.release(KEYBOARD);

        let keys = &mut p.ctx.key_generations;
        assert!(!keys.take_pending_text_input_release(keyboard, KEY_A, 2));
        assert_eq!(keys.latest_peek_sequence(KEYBOARD.seat, None), None);
        assert_eq!(
            keys.latest_peek_sequence(KEYBOARD.seat, Some(UNCREATED_SURFACE)),
            None
        );
        assert_eq!(
            keys.latest_peek_sequence(other_seat, Some(UNCREATED_SURFACE)),
            Some(3),
            "another keyboard's watermark stays"
        );
        assert!(!p.ctx.keyboard_to_extended_keyboard.contains_key(&keyboard));
        assert!(!p
            .ctx
            .extended_keyboard_to_keyboard
            .contains_key(&HostId(KEYBOARD.extended)));
        assert!(!p.ctx.keyboard_to_seat.contains_key(&KEYBOARD.guest));
    }

    /// Releasing a keyboard the proxy does not know queues no destroy.
    #[test]
    fn on_release_with_unknown_guest_id_forwards_gracefully() {
        let mut p = Proxy::new();
        p.release(Mapped {
            guest: 999,
            ..KEYBOARD
        });
        assert!(p.ctx.client_to_host_queue.is_empty());
    }

    /// Accelerator suppression and modifiers belong to their keyboard.
    #[tokio::test]
    async fn accelerator_state_is_scoped_to_its_keyboard() {
        let mut kb = Kb::with_accelerators("<Control>a").await;
        let other = kb.another_keyboard().await;
        kb.modifiers(CONTROL).await;

        assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);
        assert_eq!(
            kb.key_on(other, 3, KEY_A, RELEASED).await,
            Route::Host,
            "the other keyboard never pressed the key"
        );
        assert_eq!(kb.key(4, KEY_A, RELEASED).await, Route::Host);
        assert_eq!(
            kb.key_on(other, 5, KEY_A, PRESSED).await,
            Route::Guest,
            "the other keyboard holds no Control"
        );
        assert_eq!(kb.key(6, KEY_A, PRESSED).await, Route::Host);
    }

    /// A new keymap keeps the keys held under the old one and the modifiers
    /// Exo sent before it, which it does not send again; it ends the
    /// suppression of accelerators, whose keys may mean others now.
    #[tokio::test]
    async fn a_new_keymap_keeps_held_keys_and_modifiers() {
        let mut kb = Kb::with_accelerators("<Control>a").await;
        kb.modifiers(CONTROL).await;
        assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);
        assert_eq!(kb.key(3, KEY_B, PRESSED).await, Route::Guest);

        kb.keymap().await;
        assert_eq!(kb.key(4, KEY_B, RELEASED).await, Route::Guest);
        assert_eq!(
            kb.key(5, KEY_A, PRESSED).await,
            Route::Host,
            "the held Control survives the keymap"
        );

        kb.modifiers(0).await;
        assert_eq!(
            kb.key(6, KEY_A, PRESSED).await,
            Route::Host,
            "the accelerator's key stays suppressed"
        );
        kb.keymap().await;
        assert_eq!(kb.key(7, KEY_A, PRESSED).await, Route::Guest);
    }

    // -----------------------------------------------------------------------
    // Modifiers
    // -----------------------------------------------------------------------

    // Exo's `Keyboard::SetFocus` sends the seat's modifiers before `enter`,
    // and `WaylandKeyboardDelegate::OnKeyboardModifiers` sends them only
    // when they differ from the last ones it sent that keyboard. A key's
    // chord shows in where it goes: `<Alt>q` is the host's only with Alt
    // alone held.
    #[tokio::test]
    async fn modifiers_sent_before_enter_survive_it() {
        let accelerators = crate::accelerator::parse_accelerators("<Alt>q").unwrap();
        let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;
        kb.modifiers(ALT).await;
        let enter = wl_keyboard::evt::enter(kb.k.host, 2, kb.surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
        assert_eq!(
            kb.s.guest_received(),
            [
                entered(kb.k, 2, kb.surface.id, &[]),
                modifiers(kb.k, 2, ALT)
            ]
        );

        assert_eq!(kb.key(3, KEY_Q, PRESSED).await, Route::Host);
    }

    #[tokio::test]
    async fn modifiers_survive_a_focus_change() {
        let mut kb = Kb::with_accelerators("<Alt>q").await;
        let other_surface = kb.s.surface().await;
        kb.modifiers(ALT).await;

        // Alt stays held, so Exo sends no modifiers with the new focus.
        kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, kb.surface.host)])
            .await;
        assert_eq!(kb.s.guest_received(), [left(kb.k, 2, kb.surface.id)]);
        assert_eq!(kb.key(3, KEY_Q, PRESSED).await, Route::Host);
        assert_eq!(kb.key(4, KEY_Q, RELEASED).await, Route::Host);
        let enter = wl_keyboard::evt::enter(kb.k.host, 5, other_surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        assert_eq!(
            kb.s.guest_received(),
            [
                entered(kb.k, 5, other_surface.id, &[]),
                modifiers(kb.k, 5, ALT)
            ]
        );
        assert_eq!(kb.key(6, KEY_Q, PRESSED).await, Route::Host);
    }

    /// The protocol requires a modifiers event after every enter, which Exo
    /// sends before it, if at all.
    #[tokio::test]
    async fn every_forwarded_enter_is_followed_by_the_keyboards_modifiers() {
        let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
        let (_, other) = kb.s.keyboard().await;
        kb.modifiers(ALT).await;

        let surface = kb.surface;
        let enter = wl_keyboard::evt::enter(kb.k.host, 2, surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
        assert_eq!(
            kb.s.guest_received(),
            [entered(kb.k, 2, surface.id, &[]), modifiers(kb.k, 2, ALT)]
        );
        let other = Keyboard {
            id: other.id,
            host: other.host,
            ext: 0,
        };
        let enter = wl_keyboard::evt::enter(other.host, 3, surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        extended_keyboard_of(other, &kb.s.host_received());
        assert_eq!(
            kb.s.guest_received(),
            [entered(other, 3, surface.id, &[]), modifiers(other, 3, 0)],
            "a keyboard with no modifiers yet holds none"
        );
    }

    // Exo's `Keyboard::OnKeyboardLayoutNameChanged` sends only the keymap.
    #[tokio::test]
    async fn modifiers_carry_over_to_a_new_keymap() {
        let mut kb = Kb::with_accelerators("<Control>q").await;
        // Exo reports an active Caps Lock as depressed, not locked; a lock
        // is not part of a chord.
        kb.modifiers(CONTROL | CAPS_LOCK).await;
        kb.keymap().await;
        assert_eq!(kb.key(2, KEY_Q, PRESSED).await, Route::Host);
        // No chord holds a lock, so only the XKB state shows that the Caps
        // Lock carried over.
        let ctx = kb.s.ctx();
        assert!(ctx.keyboard_xkb_states[&HostId(kb.k.host)]
            .mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_DEPRESSED));
    }

    #[tokio::test]
    async fn modifiers_sent_before_the_keymap_apply_to_it() {
        let accelerators = crate::accelerator::parse_accelerators("<Alt>q").unwrap();
        let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;
        let id = kb.s.new_id();
        let host = kb.s.create(wl_seat::req::get_keyboard(kb.seat, id)).await;
        kb.k = Keyboard { id, host, ext: 0 };
        kb.modifiers(ALT).await;
        kb.keymap().await;
        let surface = kb.surface;
        let enter = wl_keyboard::evt::enter(kb.k.host, 2, surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
        assert_eq!(
            kb.s.guest_received(),
            [entered(kb.k, 2, surface.id, &[]), modifiers(kb.k, 2, ALT)]
        );

        assert_eq!(kb.key(3, KEY_Q, PRESSED).await, Route::Host);
    }

    /// A keyboard's modifiers end with it, even when no keymap arrived to
    /// apply them to, as every batch checks.
    #[tokio::test]
    async fn modifiers_without_a_keymap_end_with_their_keyboard() {
        let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
        let id = kb.s.new_id();
        let host = kb.s.create(wl_seat::req::get_keyboard(kb.seat, id)).await;
        let k = Keyboard { id, host, ext: 0 };
        kb.modifiers_on(k, CONTROL).await;

        kb.s.guest([wl_keyboard::req::release(id)]).await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(
            kb.s.host_received(),
            [(host, wl_keyboard::Request::Release {}.into())]
        );
    }

    // -----------------------------------------------------------------------
    // Focus
    // -----------------------------------------------------------------------

    /// A surface of the handler tests, and another.
    const SURFACE: (u32, u32) = (20, 200);
    const OTHER_SURFACE: (u32, u32) = (21, 201);

    #[tokio::test]
    async fn keyboard_enter_initializes_held_key_state() {
        let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
        let keys = BACKSPACE.to_ne_bytes();
        let enter = wl_keyboard::evt::enter(kb.k.host, 1, kb.surface.host, &keys).unwrap();
        kb.s.host([enter]).await;
        kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
        assert_eq!(
            kb.s.guest_received(),
            [
                entered(kb.k, 1, kb.surface.id, &[BACKSPACE]),
                modifiers(kb.k, 1, 0)
            ]
        );

        // The guest was told the key is held, so its release is the guest's.
        assert_eq!(kb.key(2, BACKSPACE, RELEASED).await, Route::Guest);
    }

    /// A compositor may repeat an enter for the focus a keyboard has, while
    /// a key is held and the guest composes: it changes nothing, as in
    /// ChromiumOS, which suppresses it.
    #[tokio::test]
    async fn a_duplicate_enter_changes_nothing() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 0, "한", "").unwrap();
        kb.s.host([preedit]).await;
        assert_eq!(kb.s.guest_received(), [v3_preedit(ime, "한"), done(ime)]);
        assert_eq!(kb.key(2, BACKSPACE, PRESSED).await, Route::Guest);

        let enter = wl_keyboard::evt::enter(kb.k.host, 3, kb.surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), []);

        assert_eq!(kb.key(4, BACKSPACE, RELEASED).await, Route::Guest);
        // The composition goes on.
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            [v3_preedit(ime, ""), v3_commit(ime, "한"), done(ime)]
        );
    }

    #[tokio::test]
    async fn direct_focus_replacement_sends_balanced_leave_then_enter() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let (k, surface) = (kb.k, kb.surface);
        let other_surface = kb.s.surface().await;
        let other = kb.s.keyboard_of(kb.seat).await;
        let other = Keyboard {
            id: other.id,
            host: other.host,
            ext: 0,
        };

        // Another keyboard of the seat enters another surface, which takes
        // the seat's focus without a leave.
        let enter = wl_keyboard::evt::enter(other.host, 2, other_surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        let host = kb.s.host_received();
        let other = Keyboard {
            ext: extended_keyboard_of(other, &host[..1]),
            ..other
        };
        assert!(deactivated(ime, &host[1..]), "{host:?}");
        assert_eq!(
            kb.s.guest_received(),
            [
                entered(other, 2, other_surface.id, &[]),
                modifiers(other, 2, 0),
                v3_leave(ime, surface.id),
                v3_enter(ime, other_surface.id),
            ]
        );

        // The first keyboard's delayed leave balances its enter, once.
        kb.s.host([wl_keyboard::evt::leave(k.host, 3, surface.host)])
            .await;
        assert_eq!(kb.s.guest_received(), [left(k, 3, surface.id)]);
        assert_eq!(kb.s.host_received(), []);
        kb.s.host([wl_keyboard::evt::leave(k.host, 4, surface.host)])
            .await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), []);
    }

    #[tokio::test]
    async fn leave_from_one_keyboard_keeps_same_surface_focused_for_another() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let surface = kb.surface;
        // Each keyboard gets its own enter.
        let other = kb.another_keyboard().await;

        // The first keyboard leaves, but the other one still focuses the
        // surface, so the seat's text input keeps it.
        kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, surface.host)])
            .await;
        assert_eq!(kb.s.guest_received(), [left(kb.k, 2, surface.id)]);
        assert_eq!(kb.s.host_received(), []);

        kb.s.host([wl_keyboard::evt::leave(other.host, 3, surface.host)])
            .await;
        assert_eq!(
            kb.s.guest_received(),
            [left(other, 3, surface.id), v3_leave(ime, surface.id),]
        );
        assert!(deactivated(ime, &kb.s.host_received()));
    }

    #[tokio::test]
    async fn same_keyboard_reenter_drops_delayed_old_leave() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let surface = kb.surface;
        let other_surface = kb.s.surface().await;

        let enter = wl_keyboard::evt::enter(kb.k.host, 2, other_surface.host, &[]).unwrap();
        kb.s.host([enter]).await;
        assert_eq!(
            kb.s.guest_received(),
            [
                entered(kb.k, 2, other_surface.id, &[]),
                modifiers(kb.k, 2, 0),
                v3_leave(ime, surface.id),
                v3_enter(ime, other_surface.id),
            ]
        );
        assert!(deactivated(ime, &kb.s.host_received()));

        // An old leave must not end the newer enter of the same keyboard.
        kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
            .await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), []);
    }

    #[test]
    fn unmapped_leave_does_not_clear_live_keyboard_focus_state() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.surface(SURFACE.0, SURFACE.1);
        p.focus(KEYBOARD, SURFACE.0);
        p.ctx
            .key_generations
            .observe_physical_state(HostId(KEYBOARD.host), BACKSPACE, PRESSED);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, SURFACE.0);

        // A delayed leave for a surface the guest already destroyed must
        // not tear down the live focus.
        assert_eq!(p.leave(KEYBOARD, 999), Action::Drop);
        assert!(p.held(KEYBOARD, BACKSPACE));
        assert_eq!(
            p.ctx.keyboard_focus.surface_for_seat(KEYBOARD.seat),
            Some(SURFACE.0)
        );
        assert!(p.ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn matching_leave_uses_stored_host_surface_after_mapping_is_unavailable() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, SURFACE.0);
        // The guest mapping is gone, but the focus keeps the exact host
        // surface this leave names.
        p.ctx
            .keyboard_focus
            .set_for_test(HostId(KEYBOARD.host), KEYBOARD.seat, SURFACE.0, 999);

        assert_eq!(p.leave(KEYBOARD, 999), Action::Drop);
        assert_eq!(p.ctx.keyboard_focus.surface_for_seat(KEYBOARD.seat), None);
        assert_eq!(p.ctx.text_inputs[&TEXT_INPUT.guest].active_surface, None);
        assert!(!p.ctx.text_inputs[&TEXT_INPUT.guest].host_is_active());
    }

    /// A surface keeps its mapping until the host acknowledges its destroy,
    /// so a delayed host enter for it still translates; it must not make
    /// the dead surface current again.
    #[test]
    fn delayed_enter_for_pending_destroy_surface_does_not_revive_focus() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.surface(SURFACE.0, SURFACE.1);
        p.ctx.guest_destroyed(SURFACE.0);
        assert!(p.ctx.shadow_table.is_pending_destroy_guest(SURFACE.0));

        assert_eq!(
            p.enter(KEYBOARD, SURFACE.1, &[]),
            Action::Drop,
            "the stale event must not be forwarded to the guest"
        );
        assert!(p
            .ctx
            .keyboard_focus
            .focus_for_keyboard(HostId(KEYBOARD.host))
            .is_none());
        assert!(p
            .ctx
            .keyboard_focus
            .surface_for_seat(KEYBOARD.seat)
            .is_none());
    }

    #[test]
    fn enter_without_keyboard_route_has_no_extension_side_effects() {
        let mut p = Proxy::new();
        p.ctx
            .shadow_table
            .insert_paired_for_test(KEYBOARD.guest, KEYBOARD.host, "wl_keyboard");
        p.surface(SURFACE.0, SURFACE.1);
        p.ctx.host_keyboard_extension_id = Some(HostId(99));
        p.ctx
            .shadow_table
            .insert_host_only_for_test(99, "zcr_keyboard_extension_v1", 2);

        assert_eq!(
            p.enter(KEYBOARD, SURFACE.1, &[]),
            Action::Drop,
            "an enter without a guest keyboard-to-seat route is undeliverable"
        );
        assert!(p.ctx.keyboard_to_extended_keyboard.is_empty());
        assert!(p.ctx.extended_keyboard_to_keyboard.is_empty());
        assert!(p.ctx.client_to_host_queue.is_empty());
        assert!(p
            .ctx
            .keyboard_focus
            .focus_for_keyboard(HostId(KEYBOARD.host))
            .is_none());
    }

    /// The other keyboard of a seat enters another surface while the first
    /// still holds Backspace: that enter retires the first keyboard's key
    /// state, and its later leave, mapped or not, changes nothing more.
    #[test]
    fn mapped_stale_leave_clears_old_keyboard_state_without_disturbing_new_focus() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.map(OTHER_KEYBOARD);
        p.surface(SURFACE.0, SURFACE.1);
        p.surface(OTHER_SURFACE.0, OTHER_SURFACE.1);
        p.focus(KEYBOARD, SURFACE.0);
        let keyboard = HostId(KEYBOARD.host);
        let keys = &mut p.ctx.key_generations;
        keys.observe_peek_press(keyboard, BACKSPACE, 1, 123, true);
        keys.cancel_backspace_repeat(keyboard, BACKSPACE);
        keys.suppress_host_accelerator(keyboard, BACKSPACE);
        assert!(p.ctx.claim_guest_key(keyboard, BACKSPACE, ImeRecovery));
        let held = KeyboardModifiers {
            depressed: 0xdead_beef,
            ..Default::default()
        };
        p.ctx.keyboard_modifiers.insert(keyboard, held);

        assert_eq!(
            p.enter(OTHER_KEYBOARD, OTHER_SURFACE.1, &[]),
            Action::Forward
        );
        let queued_after_enter = p.ctx.host_to_client_queue.len();
        assert_eq!(p.leave(KEYBOARD, SURFACE.1), Action::Forward);
        assert_eq!(p.ctx.host_to_client_queue.len(), queued_after_enter);

        assert!(p.ctx.keyboard_focus.focus_for_keyboard(keyboard).is_none());
        assert_eq!(
            p.ctx
                .keyboard_focus
                .focus_for_keyboard(HostId(OTHER_KEYBOARD.host))
                .map(|focus| focus.guest_surface),
            Some(OTHER_SURFACE.0)
        );
        assert_eq!(
            p.ctx.keyboard_focus.surface_for_seat(KEYBOARD.seat),
            Some(OTHER_SURFACE.0)
        );
        assert!(!p.held(KEYBOARD, BACKSPACE));
        assert!(!p.cancelled(KEYBOARD));
        assert!(p.ctx.key_generations.peek(keyboard, BACKSPACE).is_none());
        assert_eq!(p.owner(KEYBOARD, BACKSPACE), None);
        assert!(!p
            .ctx
            .key_generations
            .host_accelerator_suppressed(keyboard, BACKSPACE));
        assert!(
            p.ctx.keyboard_repeatable_keys.contains_key(&keyboard),
            "focus teardown must preserve keymap-derived repeatability"
        );
        assert_eq!(
            p.ctx.keyboard_modifiers.get(&keyboard),
            Some(&held),
            "Exo resends modifiers only when they change, so focus teardown keeps them"
        );
    }

    #[test]
    fn new_focus_retires_stale_keyboard_state_before_delayed_leave() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.map(OTHER_KEYBOARD);
        p.surface(SURFACE.0, SURFACE.1);
        p.surface(OTHER_SURFACE.0, OTHER_SURFACE.1);
        // The first keyboard held Backspace on a surface the guest since
        // destroyed.
        p.focus(KEYBOARD, OTHER_SURFACE.0);
        let keyboard = HostId(KEYBOARD.host);
        p.ctx
            .key_generations
            .observe_physical_state(keyboard, BACKSPACE, PRESSED);
        p.ctx
            .key_generations
            .cancel_backspace_repeat(keyboard, BACKSPACE);
        assert!(p.ctx.claim_guest_key(keyboard, BACKSPACE, ImeRecovery));

        assert_eq!(p.enter(OTHER_KEYBOARD, SURFACE.1, &[]), Action::Forward);
        assert_eq!(p.leave(KEYBOARD, 999), Action::Drop);

        assert!(!p.held(KEYBOARD, BACKSPACE));
        assert!(!p.cancelled(KEYBOARD));
        assert_eq!(p.owner(KEYBOARD, BACKSPACE), None);
        assert!(p
            .ctx
            .keyboard_focus
            .keyboard_owns_surface(HostId(OTHER_KEYBOARD.host), SURFACE.0));
        assert_eq!(
            p.ctx.keyboard_focus.surface_for_seat(KEYBOARD.seat),
            Some(SURFACE.0)
        );
        assert!(
            !crate::handler::text_input::backspace_pressed_for_seat(&p.ctx, KEYBOARD.seat),
            "a dead keyboard must not keep the seat's Backspace fallback armed"
        );
    }

    #[test]
    fn stale_leave_does_not_end_current_seat_backspace_repeat() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.surface(SURFACE.0, SURFACE.1);
        p.surface(OTHER_SURFACE.0, OTHER_SURFACE.1);
        p.focus(KEYBOARD, SURFACE.0);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, SURFACE.0);
        let keyboard = HostId(KEYBOARD.host);
        p.ctx
            .key_generations
            .observe_physical_state(keyboard, BACKSPACE, PRESSED);
        assert!(p
            .ctx
            .key_generations
            .arm_ime_repeat(keyboard, BACKSPACE, TEXT_INPUT.guest));
        let repeating = |p: &Proxy| {
            crate::handler::text_input::backspace_repeat_active_for_keyboard(&p.ctx, keyboard)
        };

        // The keyboard reports leaving a surface it no longer focuses.
        assert_eq!(p.leave(KEYBOARD, OTHER_SURFACE.1), Action::Drop);
        assert_eq!(
            p.ctx.keyboard_focus.surface_for_seat(KEYBOARD.seat),
            Some(SURFACE.0)
        );
        assert!(repeating(&p));
        assert!(
            p.ctx.host_to_client_queue.is_empty(),
            "stale leave must not emit a text-input leave"
        );

        // A matching leave still ends the fallback and clears focus.
        assert_eq!(p.leave(KEYBOARD, SURFACE.1), Action::Forward);
        assert_eq!(p.ctx.keyboard_focus.surface_for_seat(KEYBOARD.seat), None);
        assert!(!repeating(&p));
    }

    #[test]
    fn keyboard_enter_invalidates_stale_text_input_before_new_enable() {
        let mut p = Proxy::new();
        let keyboard = HostId(KEYBOARD.host);
        p.ctx
            .shadow_table
            .insert_paired_for_test(KEYBOARD.guest, KEYBOARD.host, "wl_keyboard");
        p.ctx.keyboard_to_seat.insert(KEYBOARD.guest, KEYBOARD.seat);
        // Text-input v1 activation names the host seat.
        p.ctx
            .shadow_table
            .insert_paired_for_test(KEYBOARD.seat, 2, "wl_seat");
        p.surface(SURFACE.0, SURFACE.1);
        let stale = {
            let mut state = crate::state::TextInputState::new(
                TEXT_INPUT.host,
                None,
                KEYBOARD.seat,
                Some(OTHER_SURFACE.0),
            );
            state.pending.enabled = Some(true);
            state.committed.enabled = true;
            state.pending.surrounding_text = Some(Some(("stale".to_string(), 5, 5)));
            state.committed.surrounding_text = Some(("stale".to_string(), 5, 5));
            state.current_preedit = "한".to_string();
            state.set_guest_commits(9);
            state.pending_deletes = vec![(3, 0)];
            state.host = crate::state::HostApplied::Active { content_type: None };
            state
        };
        p.ctx.text_inputs.insert(TEXT_INPUT.guest, stale);

        assert_eq!(p.enter(KEYBOARD, SURFACE.1, &[]), Action::Forward);

        let state = &p.ctx.text_inputs[&TEXT_INPUT.guest];
        assert_eq!(state.active_surface, Some(SURFACE.0));
        assert!(state.pending.enabled.is_none());
        assert!(!state.committed.enabled);
        assert!(!state.host_is_active());
        assert!(state.committed.surrounding_text.is_none());
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_deletes.is_empty());
        assert_eq!(state.guest_commit_serial(), 9);
        assert!(p.ctx.client_to_host_queue.iter().any(|(message, _)| {
            msg_sender(message) == TEXT_INPUT.host
                && msg_opcode(message) == zwp_text_input_v1::REQ_DEACTIVATE
        }));
        assert!(p.ctx.host_to_client_queue.iter().any(|(message, _)| {
            msg_sender(message) == TEXT_INPUT.guest
                && msg_opcode(message) == zwp_text_input_v3::EVT_ENTER
        }));

        // A Backspace held from before the enter: another key cancels its
        // IME repeat.
        p.ctx
            .key_generations
            .observe_physical_state(keyboard, BACKSPACE, PRESSED);
        assert_eq!(p.key(KEYBOARD, 2, 10, KEY_A, PRESSED), Action::Forward);
        assert!(p.held(KEYBOARD, BACKSPACE));
        assert!(p.cancelled(KEYBOARD));
        let repeating = |p: &Proxy| {
            crate::handler::text_input::backspace_repeat_active_for_keyboard(&p.ctx, keyboard)
        };
        assert!(!repeating(&p));

        // Its release clears the cancellation, so the next Backspace, which
        // the IME consumes, may repeat.
        assert_eq!(p.key(KEYBOARD, 5, 13, BACKSPACE, RELEASED), Action::Drop);
        assert!(!p.held(KEYBOARD, BACKSPACE));
        p.ctx
            .extended_keyboard_to_keyboard
            .insert(HostId(KEYBOARD.extended), keyboard);
        assert_eq!(p.peek(KEYBOARD, 6, 14, BACKSPACE, PRESSED), Action::Drop);
        assert!(p
            .ctx
            .key_generations
            .arm_ime_repeat(keyboard, BACKSPACE, TEXT_INPUT.guest));
        assert_eq!(p.key(KEYBOARD, 6, 14, BACKSPACE, PRESSED), Action::Drop);
        assert!(p.held(KEYBOARD, BACKSPACE));
        assert_eq!(p.peek(KEYBOARD, 7, 15, BACKSPACE, RELEASED), Action::Drop);
        assert_eq!(p.key(KEYBOARD, 7, 15, BACKSPACE, RELEASED), Action::Drop);
        assert!(!p.held(KEYBOARD, BACKSPACE));
        assert!(!repeating(&p));
    }

    // -----------------------------------------------------------------------
    // Keyboard release
    // -----------------------------------------------------------------------

    /// Releasing the keyboard that gave a seat its focus ends the seat's
    /// text-input focus: the guest gets its leave, and the host input
    /// method is deactivated and drained.
    #[tokio::test]
    async fn keyboard_release_ends_focus_when_no_other_keyboard_owns_the_seat() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 0, "한", "").unwrap();
        kb.s.host([preedit]).await;
        assert_eq!(kb.s.guest_received(), [v3_preedit(ime, "한"), done(ime)]);

        kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
        let surface = kb.surface.id;
        assert_eq!(kb.s.guest_received(), [v3_leave(ime, surface)]);
        let host = kb.s.host_received();
        assert_eq!(host[..2], released(kb.k));
        assert!(deactivated(ime, &host[2..]), "{host:?}");
    }

    #[tokio::test]
    async fn keyboard_release_keeps_focus_owned_by_another_keyboard() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let other = kb.another_keyboard().await;

        kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), released(kb.k));

        // The other keyboard still focuses the surface for the seat.
        let surface = kb.surface;
        kb.s.host([wl_keyboard::evt::leave(other.host, 2, surface.host)])
            .await;
        assert_eq!(
            kb.s.guest_received(),
            [left(other, 2, surface.id), v3_leave(ime, surface.id),]
        );
        assert!(deactivated(ime, &kb.s.host_received()));
    }

    #[tokio::test]
    async fn keyboard_release_after_seat_release_does_not_deactivate_destroyed_host_seat() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.s.guest([wl_seat::req::release(kb.seat)]).await;
        assert_eq!(
            kb.s.host_received(),
            [(ime.host_seat, wl_seat::Request::Release {}.into())]
        );

        kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
        let surface = kb.surface.id;
        assert_eq!(kb.s.guest_received(), [v3_leave(ime, surface)]);
        assert_eq!(
            kb.s.host_received(),
            released(kb.k),
            "no deactivate through a released host seat"
        );
    }

    #[test]
    fn keyboard_release_does_not_cancel_another_keyboard_ime_repeat() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.map(OTHER_KEYBOARD);
        p.surface(SURFACE.0, SURFACE.1);
        p.focus(KEYBOARD, SURFACE.0);
        p.focus(OTHER_KEYBOARD, SURFACE.0);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, SURFACE.0);
        let other = HostId(OTHER_KEYBOARD.host);
        p.ctx
            .key_generations
            .observe_physical_state(other, BACKSPACE, PRESSED);
        p.ctx
            .key_generations
            .cancel_backspace_repeat(other, BACKSPACE);

        p.release(KEYBOARD);
        assert!(p.held(OTHER_KEYBOARD, BACKSPACE));
        assert!(p.cancelled(OTHER_KEYBOARD));
        assert!(p.ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn keyboard_release_invalidates_all_same_seat_text_inputs_and_delayed_leave_is_not_duplicate() {
        let mut p = Proxy::new();
        p.ctx
            .shadow_table
            .insert_paired_for_test(KEYBOARD.seat, 70, "wl_seat");
        p.ctx
            .shadow_table
            .insert_paired_for_test(KEYBOARD.guest, KEYBOARD.host, "wl_keyboard");
        p.ctx.keyboard_to_seat.insert(KEYBOARD.guest, KEYBOARD.seat);
        p.surface(SURFACE.0, SURFACE.1);
        p.surface(OTHER_SURFACE.0, OTHER_SURFACE.1);
        p.focus(KEYBOARD, SURFACE.0);
        let text_inputs = [
            (40, Some(SURFACE.0)),
            (41, Some(OTHER_SURFACE.0)),
            (42, None),
        ];
        for (guest, active_surface) in text_inputs {
            p.ctx.shadow_table.insert_paired_with_version_for_test(
                guest,
                guest + 400,
                "zwp_text_input_v3",
                1,
            );
            let mut state =
                crate::state::TextInputState::new(guest + 500, None, KEYBOARD.seat, active_surface);
            state.committed.enabled = true;
            state.set_guest_commits(1);
            state.host = crate::state::HostApplied::Active { content_type: None };
            p.ctx.text_inputs.insert(guest, state);
        }

        p.release(KEYBOARD);
        assert!(p.ctx.text_inputs.values().all(|state| {
            state.active_surface.is_none()
                && state.pending.enabled.is_none()
                && !state.committed.enabled
                && !state.host_is_active()
                && state.current_preedit.is_empty()
        }));
        assert_eq!(
            p.ctx.host_to_client_queue.len(),
            2,
            "only focused text inputs should receive leave events"
        );
        let deactivated = p.ctx.client_to_host_queue.iter().filter(|(message, _)| {
            msg_opcode(message) == zwp_text_input_v1::REQ_DEACTIVATE
                && [540, 541, 542].contains(&msg_sender(message))
        });
        assert_eq!(
            deactivated.count(),
            3,
            "every active host v1 input must be deactivated, including a stale local None focus"
        );

        let guest_queue_len = p.ctx.host_to_client_queue.len();
        let host_queue_len = p.ctx.client_to_host_queue.len();
        assert_eq!(p.leave(KEYBOARD, SURFACE.1), Action::Drop);
        assert_eq!(p.ctx.host_to_client_queue.len(), guest_queue_len);
        assert_eq!(p.ctx.client_to_host_queue.len(), host_queue_len);
    }

    // -----------------------------------------------------------------------
    // Physical keys the host IME consumes
    // -----------------------------------------------------------------------

    #[test]
    fn peek_key_tracks_the_physical_key() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        let keyboard = HostId(KEYBOARD.host);

        assert_eq!(p.peek(KEYBOARD, 11, 21, BACKSPACE, REPEATED), Action::Drop);
        assert!(!p.held(KEYBOARD, BACKSPACE), "a repeat without a press");
        assert!(p.ctx.key_generations.peek(keyboard, BACKSPACE).is_none());

        assert_eq!(p.peek(KEYBOARD, 10, 20, BACKSPACE, PRESSED), Action::Drop);
        assert!(p.held(KEYBOARD, BACKSPACE));
        p.ctx
            .key_generations
            .cancel_backspace_repeat(keyboard, BACKSPACE);
        assert_eq!(p.peek(KEYBOARD, 11, 21, BACKSPACE, REPEATED), Action::Drop);
        assert!(p.held(KEYBOARD, BACKSPACE), "a repeat keeps the key down");
        assert!(
            p.cancelled(KEYBOARD),
            "a repeat must not re-arm a cancelled IME fallback"
        );

        assert_eq!(p.peek(KEYBOARD, 12, 22, BACKSPACE, RELEASED), Action::Drop);
        assert!(!p.held(KEYBOARD, BACKSPACE));
    }

    #[test]
    fn a_duplicate_pressed_peek_keeps_its_generation() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        assert_eq!(p.peek(KEYBOARD, 1, 100, KEY_SPACE, PRESSED), Action::Drop);
        assert_eq!(p.peek(KEYBOARD, 2, 200, KEY_SPACE, PRESSED), Action::Drop);
        let press = p.ctx.key_generations.peek(HostId(KEYBOARD.host), KEY_SPACE);
        assert_eq!(press.map(|press| press.sequence), Some(1));
    }

    #[test]
    fn backspace_peek_identity_survives_unrelated_release() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        assert_eq!(p.peek(KEYBOARD, 20, 200, BACKSPACE, PRESSED), Action::Drop);
        assert_eq!(p.key(KEYBOARD, 21, 210, KEY_A, RELEASED), Action::Drop);
        assert_eq!(
            p.ctx
                .key_generations
                .peek(HostId(KEYBOARD.host), BACKSPACE)
                .map(|press| (press.serial, press.time)),
            Some((20, 200)),
            "an unrelated key event must not replace the Backspace peek identity"
        );
    }

    #[test]
    fn equal_serial_peek_release_uses_timestamp_order() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        let key = KEY_T;
        let serial = 18_857;

        p.peek(KEYBOARD, serial, 100, key, PRESSED);
        p.peek(KEYBOARD, serial, 110, key, RELEASED);
        assert!(
            !p.held(KEYBOARD, key),
            "a later release must close a press even when ChromeOS reuses its serial"
        );

        p.peek(KEYBOARD, serial, 130, key, PRESSED);
        p.peek(KEYBOARD, serial, 120, key, RELEASED);
        assert!(
            p.held(KEYBOARD, key),
            "an older delayed release must not close the current generation"
        );
        p.peek(KEYBOARD, serial, 130, key, RELEASED);
        assert!(
            !p.held(KEYBOARD, key),
            "equal timestamps must remain valid for fast press/release pairs"
        );

        p.peek(KEYBOARD, serial + 2, 150, key, PRESSED);
        p.peek(KEYBOARD, serial + 1, 160, key, RELEASED);
        assert!(
            p.held(KEYBOARD, key),
            "an older serial must stay stale even when delivered with a newer timestamp"
        );
    }

    #[test]
    fn ime_repeat_state_is_scoped_to_the_originating_keyboard() {
        let mut p = Proxy::new();
        let other = Mapped {
            seat: 2,
            ..OTHER_KEYBOARD
        };
        p.map(KEYBOARD);
        p.map(other);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, UNCREATED_SURFACE);

        // The IME consumed Backspace on the first keyboard.
        p.peek(KEYBOARD, 1, 500, BACKSPACE, PRESSED);
        assert!(p.ctx.key_generations.arm_ime_repeat(
            HostId(KEYBOARD.host),
            BACKSPACE,
            TEXT_INPUT.guest
        ));
        assert_eq!(p.key(KEYBOARD, 1, 500, BACKSPACE, PRESSED), Action::Drop);

        // Its IME repeat must not swallow a Backspace of the other.
        assert_eq!(p.key(other, 2, 501, BACKSPACE, PRESSED), Action::Forward);
    }

    #[test]
    fn forwarded_backspace_release_is_not_dropped_by_ime_repeat_fallback() {
        let mut p = Proxy::new();
        p.map(KEYBOARD);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, UNCREATED_SURFACE);
        assert_eq!(p.key(KEYBOARD, 1, 500, BACKSPACE, PRESSED), Action::Forward);

        // Exo later consumes repeat confirmations. The real release still
        // has to reach the guest to close the press/release pair.
        assert!(p.ctx.key_generations.arm_ime_repeat(
            HostId(KEYBOARD.host),
            BACKSPACE,
            TEXT_INPUT.guest
        ));
        assert_eq!(
            p.key(KEYBOARD, 2, 501, BACKSPACE, RELEASED),
            Action::Forward
        );
    }

    /// A Backspace the IME repeats, which the guest got.
    fn repeating_backspace(p: &mut Proxy, forwarded: bool) {
        p.map(KEYBOARD);
        p.text_input(TEXT_INPUT, KEYBOARD.seat, UNCREATED_SURFACE);
        let keyboard = HostId(KEYBOARD.host);
        p.ctx
            .key_generations
            .observe_physical_state(keyboard, BACKSPACE, PRESSED);
        if forwarded {
            assert!(p.ctx.claim_guest_key(keyboard, BACKSPACE, Physical));
        }
        assert!(p
            .ctx
            .key_generations
            .arm_ime_repeat(keyboard, BACKSPACE, TEXT_INPUT.guest));
    }

    #[test]
    fn non_backspace_press_cancels_repeat_without_losing_physical_backspace() {
        let mut p = Proxy::new();
        repeating_backspace(&mut p, true);

        assert_eq!(p.key(KEYBOARD, 1, 500, KEY_A, PRESSED), Action::Forward);
        assert!(
            p.held(KEYBOARD, BACKSPACE),
            "pressing another key must not falsify the physical Backspace state"
        );
        assert!(
            !crate::handler::text_input::backspace_repeat_active_for_keyboard(
                &p.ctx,
                HostId(KEYBOARD.host)
            )
        );
        assert_eq!(p.confirm(TEXT_INPUT), Action::Drop);
        assert!(
            p.ctx.host_to_client_queue.is_empty(),
            "a cancelled repeat must not rearm while Backspace remains physically held"
        );

        assert_eq!(
            p.key(KEYBOARD, 2, 501, BACKSPACE, RELEASED),
            Action::Forward
        );
        assert!(!p.held(KEYBOARD, BACKSPACE));
        assert!(
            !p.cancelled(KEYBOARD),
            "Backspace release must clear repeat cancellation"
        );
    }

    #[test]
    fn backspace_auto_repeat_press_does_not_rearm_cancelled_repeat() {
        let mut p = Proxy::new();
        repeating_backspace(&mut p, false);

        // A newer key cancels the IME repeat while Backspace stays held.
        assert_eq!(p.key(KEYBOARD, 1, 500, KEY_A, PRESSED), Action::Forward);
        assert!(p.cancelled(KEYBOARD));

        // Wayland represents key auto-repeat as additional pressed events.
        // They must not clear the cancellation until the physical release.
        assert_eq!(p.key(KEYBOARD, 2, 501, BACKSPACE, PRESSED), Action::Forward);
        assert!(
            p.cancelled(KEYBOARD),
            "a repeated Backspace press must not rearm a cancelled IME repeat"
        );
        assert_eq!(p.confirm(TEXT_INPUT), Action::Drop);
        assert!(
            p.ctx.host_to_client_queue.is_empty(),
            "a repeated Backspace press must not synthesize another key pair"
        );
    }

    /// One step of [`an_empty_confirmation_recovers_the_latest_eligible_held_key`].
    #[derive(Clone, Copy, Debug)]
    enum Step {
        Peek {
            serial: u32,
            time: u32,
            key: u32,
            state: u32,
        },
        Press(u32, Route),
        Modifiers(u32),
    }

    /// The press and release an empty confirmation replays: of `key`, at
    /// `time`.
    struct Replay {
        key: u32,
        time: u32,
    }

    /// When the host IME consumes a held key, its repeat ticks arrive as
    /// empty confirmations, each of which the proxy replays to the guest
    /// as a press and release of the latest physical key the guest could
    /// type, at the time of its latest peek: synthetic events use the host
    /// compositor's time domain.
    #[tokio::test]
    async fn an_empty_confirmation_recovers_the_latest_eligible_held_key() {
        use Step::*;
        let peek = |serial, time, key, state| Peek {
            serial,
            time,
            key,
            state,
        };
        // ChromeOS may share one serial among the transitions of keys.
        let shared = 18_857;
        let cases: [(&str, Vec<Step>, Option<Replay>); 7] = [
            (
                "a held key",
                vec![peek(1, 1234, BACKSPACE, PRESSED)],
                Some(Replay {
                    key: BACKSPACE,
                    time: 1234,
                }),
            ),
            (
                "a repeated peek",
                vec![
                    peek(1, 100, KEY_SPACE, PRESSED),
                    peek(2, 200, KEY_SPACE, REPEATED),
                ],
                Some(Replay {
                    key: KEY_SPACE,
                    time: 200,
                }),
            ),
            (
                "a duplicate pressed peek",
                vec![
                    peek(1, 100, KEY_SPACE, PRESSED),
                    peek(2, 200, KEY_SPACE, PRESSED),
                ],
                Some(Replay {
                    key: KEY_SPACE,
                    time: 200,
                }),
            ),
            (
                "a key under a modifier no accelerator has",
                vec![Modifiers(SHIFT), peek(2, 500, KEY_SPACE, PRESSED)],
                Some(Replay {
                    key: KEY_SPACE,
                    time: 500,
                }),
            ),
            (
                "keys sharing a serial",
                vec![
                    peek(shared, 100, BACKSPACE, PRESSED),
                    peek(shared, 110, BACKSPACE, RELEASED),
                    peek(shared, 120, KEY_T, PRESSED),
                    peek(shared, 130, KEY_T, RELEASED),
                    peek(shared, 140, BACKSPACE, PRESSED),
                ],
                Some(Replay {
                    key: BACKSPACE,
                    time: 140,
                }),
            ),
            (
                "an accelerator, whose modifier then goes",
                vec![
                    Modifiers(CONTROL),
                    peek(2, 500, KEY_SPACE, PRESSED),
                    Modifiers(0),
                ],
                None,
            ),
            (
                "an accelerator whose peek trails its key",
                vec![
                    Modifiers(CONTROL),
                    Press(2, Route::Host),
                    Modifiers(0),
                    peek(4, 501, KEY_SPACE, PRESSED),
                ],
                None,
            ),
        ];
        for (case, steps, recovery) in cases {
            let mut kb = Kb::with_accelerators("<Control>space").await;
            let ime = kb.text_input().await;
            for step in steps {
                match step {
                    Peek {
                        serial,
                        time,
                        key,
                        state,
                    } => kb.peek(serial, time, key, state).await,
                    Press(serial, route) => {
                        assert_eq!(kb.key(serial, KEY_SPACE, PRESSED).await, route, "{case}")
                    }
                    Modifiers(depressed) => kb.modifiers(depressed).await,
                }
            }
            kb.confirm(ime).await;
            let expected = match recovery {
                Some(Replay { key, time }) => {
                    let [press, release] = recovered(kb.k, 1, time, key);
                    vec![press, release, done(ime)]
                }
                None => Vec::new(),
            };
            assert_eq!(kb.s.guest_received(), expected, "{case}");
        }
    }

    #[tokio::test]
    async fn held_space_repeats_after_ime_commit_until_physical_release() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;

        // Captured ChromeOS sequence: the host IME consumes the physical
        // Space and includes the first one in its committed string. Further
        // repeat ticks arrive only as empty confirm_preedit events.
        kb.peek(700, 1234, KEY_SPACE, PRESSED).await;
        let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 1, "가", "").unwrap();
        let commit = zwp_text_input_v1::evt::commit_string(ime.host, 1, "가 ").unwrap();
        kb.s.host([preedit, commit]).await;
        assert_eq!(
            kb.s.guest_received(),
            [
                v3_preedit(ime, "가"),
                done(ime),
                v3_preedit(ime, ""),
                v3_commit(ime, "가 "),
                done(ime),
            ],
            "the first Space commits once"
        );

        for serial in [1, 3, 5] {
            kb.confirm(ime).await;
            let [press, release] = recovered(kb.k, serial, 1234, KEY_SPACE);
            assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);
        }

        kb.peek(701, 1300, KEY_SPACE, RELEASED).await;
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            [],
            "a confirmation after the physical release changes nothing"
        );
    }

    /// The delayed wl_keyboard events of a key that IME recovery replayed
    /// stay with the host, until the key's next press.
    #[tokio::test]
    async fn delayed_keyboard_events_do_not_duplicate_an_ime_recovered_key() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        kb.confirm(ime).await;
        let [press, release] = recovered(kb.k, 1, 500, KEY_SPACE);
        assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);

        assert_eq!(kb.key(1, KEY_SPACE, PRESSED).await, Route::Host);
        assert_eq!(kb.key(3, KEY_SPACE, REPEATED).await, Route::Host);
        kb.peek(4, 503, KEY_SPACE, RELEASED).await;
        assert_eq!(kb.key(4, KEY_SPACE, RELEASED).await, Route::Host);

        assert_eq!(
            kb.key(5, KEY_SPACE, PRESSED).await,
            Route::Guest,
            "the next physical generation retires the recovery"
        );
    }

    #[tokio::test]
    async fn delayed_keysym_events_do_not_duplicate_an_ime_recovered_key() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        kb.confirm(ime).await;
        let [press, release] = recovered(kb.k, 1, 500, KEY_SPACE);
        assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);

        kb.keysym(ime, 501, 2, keysyms::KEY_space, PRESSED).await;
        kb.keysym(ime, 502, 3, keysyms::KEY_space, RELEASED).await;
        kb.peek(4, 503, KEY_SPACE, RELEASED).await;
        assert_eq!(kb.s.guest_received(), []);
    }

    /// Whatever order the delayed channels of a key that IME recovery
    /// replayed arrive in, none reaches the guest; the key's next physical
    /// generation does.
    #[tokio::test]
    async fn recovery_tombstone_suppresses_keysym_after_physical_release() {
        #[derive(Clone, Copy, Debug)]
        enum Delayed {
            KeyboardPress,
            KeyboardRelease,
            PeekRelease,
            KeysymPress,
            KeysymRelease,
        }
        use Delayed::*;

        let orderings = [
            [
                KeysymPress,
                KeysymRelease,
                KeyboardPress,
                KeyboardRelease,
                PeekRelease,
            ],
            [
                KeyboardPress,
                KeysymPress,
                KeysymRelease,
                KeyboardRelease,
                PeekRelease,
            ],
            [
                KeyboardPress,
                KeyboardRelease,
                KeysymPress,
                KeysymRelease,
                PeekRelease,
            ],
            [
                KeysymPress,
                KeyboardPress,
                PeekRelease,
                KeysymRelease,
                KeyboardRelease,
            ],
            [
                KeyboardPress,
                PeekRelease,
                KeysymPress,
                KeysymRelease,
                KeyboardRelease,
            ],
            [
                KeysymPress,
                KeyboardPress,
                KeyboardRelease,
                PeekRelease,
                KeysymRelease,
            ],
            [
                PeekRelease,
                KeyboardPress,
                KeyboardRelease,
                KeysymPress,
                KeysymRelease,
            ],
        ];
        for ordering in orderings {
            let mut kb = Kb::new().await;
            let ime = kb.text_input().await;
            kb.peek(1, 500, KEY_SPACE, PRESSED).await;
            kb.confirm(ime).await;
            let [press, release] = recovered(kb.k, 1, 500, KEY_SPACE);
            assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);

            for event in ordering {
                match event {
                    KeyboardPress => {
                        assert_eq!(kb.key(1, KEY_SPACE, PRESSED).await, Route::Host)
                    }
                    KeyboardRelease => {
                        assert_eq!(kb.key(2, KEY_SPACE, RELEASED).await, Route::Host)
                    }
                    PeekRelease => kb.peek(2, 503, KEY_SPACE, RELEASED).await,
                    KeysymPress => kb.keysym(ime, 500, 1, keysyms::KEY_space, PRESSED).await,
                    KeysymRelease => kb.keysym(ime, 503, 2, keysyms::KEY_space, RELEASED).await,
                }
                assert_eq!(kb.s.guest_received(), [], "{ordering:?}: {event:?}");
            }

            kb.peek(6, 600, KEY_SPACE, PRESSED).await;
            kb.keysym(ime, 601, 7, keysyms::KEY_space, PRESSED).await;
            kb.keysym(ime, 602, 8, keysyms::KEY_space, RELEASED).await;
            assert_eq!(
                kb.s.guest_received(),
                [
                    key_event(kb.k, 7, 601, KEY_SPACE, PRESSED),
                    key_event(kb.k, 8, 602, KEY_SPACE, RELEASED)
                ],
                "{ordering:?}"
            );
        }
    }

    #[tokio::test]
    async fn stale_keyboard_press_cannot_steal_a_new_peek_generation() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        kb.confirm(ime).await;
        kb.s.guest_received();
        kb.peek(2, 510, KEY_SPACE, RELEASED).await;
        kb.peek(10, 600, KEY_SPACE, PRESSED).await;

        assert_eq!(
            kb.key(1, KEY_SPACE, PRESSED).await,
            Route::Host,
            "the prior generation's serial must not open the current key"
        );
        assert_eq!(
            kb.key(10, KEY_SPACE, PRESSED).await,
            Route::Guest,
            "the current peek generation must still accept its matching key"
        );
    }

    #[tokio::test]
    async fn delayed_release_closes_only_its_retired_physical_generation() {
        let mut kb = Kb::new().await;
        kb.peek(1, 500, KEY_A, PRESSED).await;
        assert_eq!(kb.key(1, KEY_A, PRESSED).await, Route::Guest);
        kb.peek(2, 510, KEY_A, RELEASED).await;
        kb.peek(10, 600, KEY_A, PRESSED).await;
        assert_eq!(kb.key(10, KEY_A, PRESSED).await, Route::Guest);

        assert_eq!(
            kb.key(2, KEY_A, RELEASED).await,
            Route::Guest,
            "the delayed release must close the retired guest press"
        );
        kb.peek(11, 610, KEY_A, RELEASED).await;
        assert_eq!(
            kb.key(11, KEY_A, RELEASED).await,
            Route::Guest,
            "the current generation kept its press"
        );
    }

    /// Text-input may deliver the keysyms of a key while peek_key alone
    /// tracks that it is held: the IME may still recover it, once.
    #[tokio::test]
    async fn keysym_release_does_not_end_a_physical_ime_consumed_generation() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        kb.keysym(ime, 501, 2, keysyms::KEY_space, PRESSED).await;
        kb.keysym(ime, 502, 3, keysyms::KEY_space, RELEASED).await;
        assert_eq!(
            kb.s.guest_received(),
            [
                key_event(kb.k, 2, 501, KEY_SPACE, PRESSED),
                key_event(kb.k, 3, 502, KEY_SPACE, RELEASED)
            ]
        );

        assert_eq!(
            kb.key(1, KEY_SPACE, PRESSED).await,
            Route::Host,
            "the keysyms suppress the delayed physical press"
        );
        kb.confirm(ime).await;
        let [press, release] = recovered(kb.k, 1, 500, KEY_SPACE);
        assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);
        kb.peek(4, 503, KEY_SPACE, RELEASED).await;
        kb.confirm(ime).await;
        assert_eq!(kb.s.guest_received(), []);
    }

    #[tokio::test]
    async fn keysym_and_repeat_recovery_share_the_latest_keyboard_owner() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let other = kb.another_keyboard().await;
        kb.peek_on(other, 1, 500, KEY_SPACE, PRESSED).await;

        kb.keysym(ime, 501, 2, keysyms::KEY_space, PRESSED).await;
        kb.keysym(ime, 502, 3, keysyms::KEY_space, RELEASED).await;
        assert_eq!(
            kb.s.guest_received(),
            [
                key_event(other, 2, 501, KEY_SPACE, PRESSED),
                key_event(other, 3, 502, KEY_SPACE, RELEASED)
            ],
            "keysyms go to the keyboard of the latest matching peek"
        );

        kb.confirm(ime).await;
        let [press, release] = recovered(other, 1, 500, KEY_SPACE);
        assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);
        kb.keysym(ime, 503, 4, keysyms::KEY_space, PRESSED).await;
        kb.keysym(ime, 504, 5, keysyms::KEY_space, RELEASED).await;
        assert_eq!(
            kb.s.guest_received(),
            [],
            "delayed keysyms see the recovery of the same keyboard"
        );
    }

    #[tokio::test]
    async fn newer_key_on_another_keyboard_remains_the_causal_watermark_after_release() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let other = kb.another_keyboard().await;
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        // Shift does not repeat.
        kb.peek_on(other, 2, 501, KEY_LEFTSHIFT, PRESSED).await;
        kb.confirm(ime).await;
        assert_eq!(kb.s.guest_received(), []);

        kb.peek_on(other, 3, 502, KEY_LEFTSHIFT, RELEASED).await;
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            [],
            "releasing a newer key must not revive an older held generation"
        );
    }

    #[tokio::test]
    async fn keyboard_retirement_removes_its_watermark_and_recovers_other_held_key() {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        let other = kb.another_keyboard().await;
        // The first keyboard holds Space; the other is then the latest
        // with a key that does not repeat.
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        kb.peek_on(other, 2, 501, KEY_LEFTSHIFT, PRESSED).await;
        kb.confirm(ime).await;
        assert_eq!(kb.s.guest_received(), []);

        // The other keyboard ends, and its key with it.
        kb.s.guest([wl_keyboard::req::release(other.id)]).await;
        assert_eq!(kb.s.guest_received(), []);
        assert_eq!(kb.s.host_received(), released(other));
        kb.confirm(ime).await;
        let [press, release] = recovered(kb.k, 1, 500, KEY_SPACE);
        assert_eq!(kb.s.guest_received(), [press, release, done(ime)]);
    }
}
