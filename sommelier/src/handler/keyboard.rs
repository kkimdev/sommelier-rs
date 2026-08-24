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
//!
//! See `docs/KEYBOARD_SHORTCUT_INHIBITION.md` for the full protocol flow.

use crate::protocols::aura_shell::zaura_surface::{REQ_SET_PARENT, REQ_UNSET_SNAP};
use crate::protocols::aura_shell::zaura_toplevel::REQ_SET_WINDOW_BOUNDS;
use crate::protocols::wayland::wl_keyboard;
use crate::protocols::xdg_shell::xdg_toplevel::{REQ_UNSET_FULLSCREEN, REQ_UNSET_MAXIMIZED};
use crate::state::{
    Context, GuestId, GuestKeyDelivery, GuestKeyEvent, GuestKeyOwner, HostId, KeyboardFocus,
};
use crate::window_shortcuts::{ShortcutConfig, WindowShortcut};
use crate::wire::{Action, MessageBuilder};
use xkbcommon::xkb;

/// `wl_keyboard.key` state values (Wayland spec §wl_keyboard.key).
pub(crate) const WL_KEY_PRESSED: u32 = 1;
pub(crate) const WL_KEY_RELEASED: u32 = 0;
/// Since wl_keyboard version 10, compositors may emit repeated instead of
/// pressed for compositor-driven key repetition. It has the same physical
/// state as pressed but remains a distinct event on the wire.
pub(crate) const WL_KEY_REPEATED: u32 = 2;

/// Linux evdev keycode reported by wl_keyboard and peek_key for Backspace.
pub(crate) const EVDEV_KEY_BACKSPACE: u32 = 14;

/// `wl_keyboard.keymap` format value for XKB (Wayland spec §wl_keyboard.keymap_format).
const WL_KEYMAP_FORMAT_NO_KEYMAP: u32 = 0;
const WL_KEYMAP_FORMAT_XKB_V1: u32 = 1;

// Opcodes are taken directly from the XML protocol definition:
// third_party/protocols/keyboard-extension-unstable-v1.xml
//
// zcr_extended_keyboard_v1 requests:
//   request index 0 = destroy
//   request index 1 = ack_key
// zcr_keyboard_extension_v1 requests:
//   request index 0 = get_extended_keyboard
//
// These are validated against the generated protocol code in the
// `opcode_constants_match_generated_protocol` test below. If the XML
// is ever updated, update both the constants and the test.
const ZCR_EXTENDED_KEYBOARD_DESTROY: u16 = 0;
const ZCR_EXTENDED_KEYBOARD_ACK_KEY: u16 = 1;
const ZCR_KEYBOARD_EXTENSION_GET_EXTENDED_KEYBOARD: u16 = 0;

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
    fn from_fd(fd: std::os::unix::io::RawFd, len: usize) -> Option<Self> {
        use nix::sys::mman::{mmap, MapFlags, ProtFlags};
        use nix::sys::stat::{fstat, SFlag};
        use std::os::unix::io::BorrowedFd;

        // `BorrowedFd::borrow_raw` requires a valid non-negative descriptor.
        // Check before constructing it so malformed protocol input cannot
        // create an invalid borrowed lifetime or reach fstat/mmap with -1.
        if fd < 0 {
            log::error!("on_keymap: rejecting negative fd={}", fd);
            return None;
        }
        let nonzero_len = std::num::NonZeroUsize::new(len)?;
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
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

/// Keyboard handler that tracks XKB state for keysym resolution and sends
/// `ack_key` responses to the host via `zcr_extended_keyboard_v1`.
pub struct KeyboardHandler {
    context: xkb::Context,
    /// Parsed keymap for each host keyboard. Keymap updates and failures are
    /// scoped to their sender so one seat cannot invalidate another seat.
    keymaps: std::collections::HashMap<HostId, xkb::Keymap>,
    /// XKB state for each host keyboard. A client can bind keyboards from
    /// multiple seats, whose modifier and layout-group state must not leak.
    states: std::collections::HashMap<HostId, xkb::State>,
    /// Current modifier bitmask for each host keyboard (using accelerator.rs
    /// conventions).
    modifiers: std::collections::HashMap<HostId, u32>,
    /// Statically enforce `!Sync`: `KeyboardHandler` must never be shared
    /// across threads. `xkb::State` uses non-atomic interior mutation.
    _not_sync: std::marker::PhantomData<*mut ()>,
}

impl KeyboardHandler {
    pub fn new() -> Self {
        Self {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            keymaps: std::collections::HashMap::new(),
            states: std::collections::HashMap::new(),
            modifiers: std::collections::HashMap::new(),
            _not_sync: std::marker::PhantomData,
        }
    }

    fn guest_seat_for_host_keyboard(ctx: &Context, host_keyboard_id: HostId) -> Option<u32> {
        let guest_keyboard_id = ctx.shadow_table.guest_id_of(host_keyboard_id)?.0;
        ctx.keyboard_to_seat.get(&guest_keyboard_id).copied()
    }

    fn clear_host_keyboard_state(ctx: &mut Context, host_keyboard_id: HostId) {
        ctx.key_generations.clear_keyboard(host_keyboard_id);
    }

    fn retire_focus_scoped_keyboard_state(&mut self, ctx: &mut Context, host_keyboard_id: HostId) {
        Self::clear_host_keyboard_state(ctx, host_keyboard_id);
        self.reset_host_keyboard_modifiers(host_keyboard_id);
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

    /// Check if the pressed key matches any configured host accelerators.
    fn is_host_accelerator(
        &self,
        host_keyboard_id: HostId,
        accelerators: &[crate::accelerator::Accelerator],
        key: u32,
    ) -> bool {
        let Some(state) = self.states.get(&host_keyboard_id) else {
            return false;
        };
        let modifiers = self.modifiers.get(&host_keyboard_id).copied().unwrap_or(0);

        let Some(xkb_raw_keycode) = key.checked_add(8) else {
            log::warn!("Ignoring overflowing evdev keycode {}", key);
            return false;
        };
        let xkb_keycode = xkb::Keycode::new(xkb_raw_keycode);
        // Use key_get_one_sym so that the full XKB state (including active shift
        // level) is considered. key_get_syms_by_level at level 0 would always
        // return the unshifted symbol, causing <Shift>-modified accelerators to
        // fail to match when Shift is held.
        //
        // key_get_one_sym returns KEY_NoSymbol when more than one keysym is
        // mapped to this key at the current shift level (e.g. dead keys or
        // Unicode combining sequences). In that case we conservatively return
        // false (do not treat the key as a host accelerator), which is the
        // correct safe default: we'd rather forward an unknown key to the guest
        // than accidentally suppress it.
        let sym = state.key_get_one_sym(xkb_keycode);
        if sym.raw() == xkb::keysyms::KEY_NoSymbol {
            return false;
        }
        let lower_sym = crate::accelerator::keysym_to_lower(sym.raw());
        for acc in accelerators {
            if modifiers == acc.modifiers && lower_sym == acc.symbol {
                log::trace!(
                    "Accelerator match: keyboard={}, key={}, modifiers={:#x}, sym={:#x}",
                    host_keyboard_id.0,
                    key,
                    modifiers,
                    lower_sym
                );
                return true;
            }
        }
        false
    }

    /// Resolve the focused guest xdg_toplevel and its backing wl_surface.
    fn active_xdg_toplevel(ctx: &Context, host_keyboard_id: HostId) -> Option<(u32, u32)> {
        let guest_keyboard_id = ctx.shadow_table.guest_id_of(host_keyboard_id)?.0;
        let guest_seat_id = *ctx.keyboard_to_seat.get(&guest_keyboard_id)?;
        let active_surface = ctx.keyboard_focus.surface_for_seat(guest_seat_id)?;
        if !ctx
            .keyboard_focus
            .keyboard_owns_surface(host_keyboard_id, active_surface)
        {
            return None;
        }
        ctx.xdg_toplevel_to_wl_surface
            .iter()
            .find_map(|(&xdg_toplevel_id, &surface_id)| {
                (surface_id == active_surface).then_some((xdg_toplevel_id, surface_id))
            })
    }

    fn window_shortcut(
        &self,
        host_keyboard_id: HostId,
        key: u32,
        config: &ShortcutConfig,
    ) -> Option<WindowShortcut> {
        let state = self.states.get(&host_keyboard_id)?;
        let modifiers = self.modifiers.get(&host_keyboard_id).copied().unwrap_or(0);
        let xkb_raw_keycode = key.checked_add(8)?;
        let sym = state.key_get_one_sym(xkb::Keycode::new(xkb_raw_keycode));
        log::trace!(
            "window shortcut candidate: keyboard={} key={} sym={:#x} modifiers={:#x}",
            host_keyboard_id.0,
            key,
            sym.raw(),
            modifiers
        );
        let accelerator = crate::accelerator::Accelerator {
            modifiers,
            symbol: crate::accelerator::keysym_to_lower(sym.raw()),
        };
        let shortcut = config.find(accelerator);
        log::trace!("window shortcut candidate resolved to {:?}", shortcut);
        shortcut
    }

    fn queue_xdg_request(ctx: &mut Context, host_xdg_toplevel_id: u32, opcode: u16) {
        let message = MessageBuilder::new().build_message(host_xdg_toplevel_id, opcode);
        ctx.client_to_host_queue.push((message, Vec::new()));
    }

    fn clear_window_state(ctx: &mut Context, host_xdg_toplevel_id: u32, zaura_surface_id: u32) {
        Self::queue_xdg_request(ctx, host_xdg_toplevel_id, REQ_UNSET_FULLSCREEN);
        Self::queue_xdg_request(ctx, host_xdg_toplevel_id, REQ_UNSET_MAXIMIZED);
        let message = MessageBuilder::new().build_message(zaura_surface_id, REQ_UNSET_SNAP);
        ctx.client_to_host_queue.push((message, Vec::new()));
    }

    fn apply_window_layout(
        ctx: &mut Context,
        host_keyboard_id: HostId,
        shortcut: WindowShortcut,
    ) -> bool {
        if !ctx.window_placement.handles_shortcuts() {
            log::trace!(
                "window shortcut {:?} ignored: geometry method is disabled",
                shortcut
            );
            return false;
        }
        let Some((guest_xdg_toplevel_id, guest_wl_surface_id)) =
            Self::active_xdg_toplevel(ctx, host_keyboard_id)
        else {
            log::debug!(
                "window shortcut {:?} ignored: no active xdg_toplevel for keyboard {}",
                shortcut,
                host_keyboard_id.0
            );
            return false;
        };
        let Some(host_xdg_toplevel_id) = ctx.shadow_table.get_host_id(guest_xdg_toplevel_id) else {
            log::debug!(
                "window shortcut {:?} ignored: guest xdg_toplevel {} has no host mapping",
                shortcut,
                guest_xdg_toplevel_id
            );
            return false;
        };
        let Some((output_host_id, output)) = ctx.primary_output() else {
            log::debug!(
                "window shortcut {:?} ignored: no usable output for xdg_toplevel {}",
                shortcut,
                guest_xdg_toplevel_id
            );
            return false;
        };
        let Some((x, y, width, height)) = shortcut.rect.to_bounds(
            output
                .work_area()
                .expect("primary_output only returns an output with a work area"),
        ) else {
            log::debug!(
                "window shortcut {:?} ignored: invalid output geometry {:?}",
                shortcut,
                output
            );
            return false;
        };
        let Some(zaura_toplevel_id) =
            crate::handler::compositor::ensure_zaura_toplevel(ctx, guest_xdg_toplevel_id)
        else {
            log::debug!(
                "window shortcut {:?} ignored: no zaura_toplevel for xdg_toplevel {}",
                shortcut,
                guest_xdg_toplevel_id
            );
            return false;
        };
        let Some(zaura_surface_id) =
            crate::handler::compositor::ensure_host_zaura_surface(ctx, guest_wl_surface_id)
        else {
            log::debug!(
                "window shortcut {:?} ignored: no zaura_surface for wl_surface {}",
                shortcut,
                guest_wl_surface_id
            );
            return false;
        };

        // The self-parent path is a position-only experiment. If both
        // experimental flags are present, prefer the ARC-session bounds path:
        // it is the only path that carries width/height and therefore the
        // only one that can implement a real grid resize.
        if ctx.window_placement.uses_self_parent() {
            let zaura_surface_version = ctx
                .shadow_table
                .host_object_version(zaura_surface_id)
                .unwrap_or(ctx.host_zaura_shell_version);
            if zaura_surface_version < 2 {
                log::warn!(
                    "window layout {:?}: self-parent probe requires zaura_surface v2, got v{}",
                    shortcut,
                    zaura_surface_version
                );
                return false;
            }

            let Some((origin_x, origin_y)) = ctx.window_placement.origin(zaura_toplevel_id) else {
                log::debug!(
                    "window layout {:?}: no screen origin is known for zaura_toplevel {}; \
                     consuming shortcut until configure/origin_change arrives",
                    shortcut,
                    zaura_toplevel_id
                );
                // Do not forward an early Alt+layout key to the guest. Until
                // the first screen-coordinate configure arrives, forwarding
                // it lets ChromeOS interpret the same chord as a native
                // accelerator, which can move the window through an unrelated
                // snap/restore path.
                return true;
            };
            let Some(relative_x) = x.checked_sub(origin_x) else {
                log::warn!(
                    "window layout {:?}: x coordinate overflow converting target {} from origin {}",
                    shortcut,
                    x,
                    origin_x
                );
                return false;
            };
            let Some(relative_y) = y.checked_sub(origin_y) else {
                log::warn!(
                    "window layout {:?}: y coordinate overflow converting target {} from origin {}",
                    shortcut,
                    y,
                    origin_y
                );
                return false;
            };

            // Exo's self-parent cycle does not reliably emit origin_change
            // (the cycle is rejected before the normal parent notification),
            // so predict the new screen origin for a rapid second shortcut.
            // A later configure/origin_change replaces this prediction with
            // the compositor's authoritative value. The state object also
            // verifies that the Aura child still belongs to a live XDG role
            // before accepting this mutation.
            if !ctx
                .window_placement
                .predict_origin(zaura_toplevel_id, (x, y))
            {
                log::warn!(
                    "window layout {:?}: Aura toplevel {} was released before \
                     self-parent prediction",
                    shortcut,
                    zaura_toplevel_id
                );
                return true;
            }
            // This deliberately uses the same surface as both child and
            // parent. Chromium's Exo implementation rejects the transient
            // cycle, but still runs the coordinate calculation; the probe is
            // useful for ordinary placement on a custom host build. The
            // position is relative to the current contents-view origin, not
            // an absolute screen coordinate.
            let mut builder = MessageBuilder::new();
            builder.write_u32(zaura_surface_id);
            builder.write_i32(relative_x);
            builder.write_i32(relative_y);
            let message = builder.build_message(zaura_surface_id, REQ_SET_PARENT);
            ctx.client_to_host_queue.push((message, Vec::new()));
            if !crate::handler::compositor::queue_window_placement_barrier(ctx, zaura_toplevel_id) {
                log::warn!(
                    "window layout {:?}: failed to queue host sync barrier for self-parent probe",
                    shortcut
                );
            }
            log::warn!(
                "window layout {:?}: experimental self-parent probe sent for zaura_surface={} \
                 target_screen_position=({}, {}) origin=({}, {}) \
                 relative_position=({}, {})",
                shortcut,
                zaura_surface_id,
                x,
                y,
                origin_x,
                origin_y,
                relative_x,
                relative_y
            );
            log::warn!(
                "window layout {:?}: self-parent is position-only; requested grid size \
                 {}x{} is not sent because zaura_surface.set_parent has no size argument",
                shortcut,
                width,
                height
            );
            return true;
        }

        if !ctx.window_placement.uses_bounds() {
            log::debug!(
                "window shortcut {:?} ignored: geometry method does not support bounds",
                shortcut
            );
            return false;
        }

        Self::clear_window_state(ctx, host_xdg_toplevel_id, zaura_surface_id);
        let mut builder = MessageBuilder::new();
        builder.write_i32(x);
        builder.write_i32(y);
        builder.write_i32(width);
        builder.write_i32(height);
        builder.write_u32(output_host_id);
        let message = builder.build_message(zaura_toplevel_id, REQ_SET_WINDOW_BOUNDS);
        ctx.client_to_host_queue.push((message, Vec::new()));
        if !crate::handler::compositor::queue_window_placement_barrier(ctx, zaura_toplevel_id) {
            log::warn!(
                "window layout {:?}: failed to queue host sync barrier for zaura_toplevel {}",
                shortcut,
                zaura_toplevel_id
            );
        }
        log::info!(
            "window layout {:?}: xdg_toplevel={} zaura_toplevel={} bounds=({}, {}, {}, {}) output={}",
            shortcut,
            guest_xdg_toplevel_id,
            zaura_toplevel_id,
            x,
            y,
            width,
            height,
            output_host_id
        );
        true
    }

    fn reset_host_keyboard_modifiers(&mut self, host_keyboard_id: HostId) {
        self.modifiers.remove(&host_keyboard_id);
        if let Some(state) = self.states.get_mut(&host_keyboard_id) {
            state.update_mask(0, 0, 0, 0, 0, 0);
        }
    }

    fn clear_host_keyboard_keymap(&mut self, ctx: &mut Context, host_keyboard_id: HostId) {
        self.keymaps.remove(&host_keyboard_id);
        self.states.remove(&host_keyboard_id);
        self.modifiers.remove(&host_keyboard_id);
        ctx.keyboard_repeatable_keys.remove(&host_keyboard_id);
    }

    fn clear_host_keyboard_keymap_and_state(
        &mut self,
        ctx: &mut Context,
        host_keyboard_id: HostId,
    ) {
        self.clear_host_keyboard_keymap(ctx, host_keyboard_id);
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
                let host_extended_id = HostId::from_allocated(ctx.shadow_table.allocate_host_id());
                ctx.keyboard_to_extended_keyboard
                    .insert(host_keyboard_id, host_extended_id);
                ctx.extended_keyboard_to_keyboard
                    .insert(host_extended_id, host_keyboard_id);
                let extended_version = ctx
                    .shadow_table
                    .host_object_version(extension_host_id.0)
                    .unwrap_or(u32::MAX);
                ctx.shadow_table.track_host_interface_with_version(
                    host_extended_id.0,
                    "zcr_extended_keyboard_v1".to_string(),
                    extended_version,
                );

                // zcr_keyboard_extension_v1.get_extended_keyboard(new_id, keyboard)
                // payload = [new_id(4)][keyboard(4)] = 8 bytes.
                let mut builder = crate::wire::MessageBuilder::new();
                builder.write_u32(host_extended_id.0);
                builder.write_u32(host_keyboard_id.0);
                let msg = builder.build_message(
                    extension_host_id.0,
                    ZCR_KEYBOARD_EXTENSION_GET_EXTENDED_KEYBOARD,
                );
                ctx.client_to_host_queue.push((msg, Vec::new()));
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
        // zcr_extended_keyboard_v1.ack_key — payload = [serial(4)][handled(4)].
        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_u32(serial);
        builder.write_u32(if handled { 1u32 } else { 0u32 });
        let msg = builder.build_message(host_extended_id.0, ZCR_EXTENDED_KEYBOARD_ACK_KEY);
        ctx.client_to_host_queue.push((msg, Vec::new()));
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
        fd: std::os::unix::io::RawFd,
        size: u32,
    ) -> Action {
        let host_keyboard_id = HostId::from_event_sender(ctx);
        if format == WL_KEYMAP_FORMAT_NO_KEYMAP {
            self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }
        // Only handle XKB_V1 format keymaps.
        if format != WL_KEYMAP_FORMAT_XKB_V1 {
            log::warn!("on_keymap: unsupported keymap format {}, ignoring", format);
            self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }

        // A zero-size keymap is malformed; mmap(len=0) is UB per POSIX.
        if size == 0 {
            log::warn!("on_keymap: received zero-size keymap from host, ignoring");
            self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
            return Action::Forward;
        }

        // size is a u32 from the Wayland wire; the cast to usize is lossless on
        // 64-bit Linux (the only supported target for sommelier).
        let Some(mapping) = MmapView::from_fd(fd, size as usize) else {
            log::error!("on_keymap: mmap failed for fd={}, size={}", fd, size);
            self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
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
            self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
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
                self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
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
                    self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
                }
                Some(keymap) => {
                    // Replacing the XKB keymap must not end the current
                    // physical-key session. Wayland may resend a keymap while
                    // keys are held; clearing the forwarded/pressed sets here
                    // would make the later releases disappear and leave the
                    // guest with stuck keys. Reset only XKB interpretation
                    // state and accelerator drop bookkeeping.
                    self.clear_host_keyboard_keymap(ctx, host_keyboard_id);
                    ctx.keyboard_keysym_to_keycode.remove(&host_keyboard_id);
                    let keysym_map = Self::keysym_to_evdev_keycodes(&keymap);
                    let repeatable_keys = Self::repeatable_evdev_keycodes(&keymap);
                    self.states
                        .insert(host_keyboard_id, xkb::State::new(&keymap));
                    self.modifiers.remove(&host_keyboard_id);
                    self.keymaps.insert(host_keyboard_id, keymap);
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
        _serial: u32,
        surface: u32, // Host ID
        keys: &[u8],
    ) -> Action {
        // on_enter is a host→client event: last_sender_id is the host keyboard ID.
        let host_keyboard_id = HostId::from_event_sender(ctx);
        let guest_keyboard_id = ctx
            .shadow_table
            .guest_id_of(host_keyboard_id)
            .map(|g| g.0)
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
            self.retire_focus_scoped_keyboard_state(ctx, *retired_keyboard);
        }
        Self::initialize_host_keyboard_enter_state(ctx, host_keyboard_id, keys);
        self.reset_host_keyboard_modifiers(host_keyboard_id);

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
            .guest_id_of(host_keyboard_id)
            .map(|g| g.0)
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
            self.retire_focus_scoped_keyboard_state(ctx, *retired_keyboard);
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
            .guest_id_of(host_keyboard_id)
            .map(|g| g.0)
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
        if state == WL_KEY_RELEASED
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
        if state == WL_KEY_RELEASED
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
        if state == WL_KEY_PRESSED
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
        if state == WL_KEY_PRESSED {
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
        if state == WL_KEY_RELEASED
            || (state == WL_KEY_REPEATED
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

        // Take one immutable snapshot for this event. A reload can replace the
        // handle concurrently with key delivery; the snapshot keeps the press
        // decision internally consistent. Repeat/release deliberately consult
        // the key owner instead of resolving the chord again, so a reload
        // cannot strand a key whose press was already consumed.
        let config = ctx.shortcut_config.snapshot();
        let shortcut = (state == WL_KEY_PRESSED)
            .then(|| self.window_shortcut(host_keyboard_id, key, &config))
            .flatten();
        let compositor_shortcut = match state {
            WL_KEY_PRESSED => shortcut
                .is_some_and(|shortcut| Self::apply_window_layout(ctx, host_keyboard_id, shortcut)),
            WL_KEY_REPEATED => {
                ctx.guest_key_owner(host_keyboard_id, key)
                    == Some(GuestKeyOwner::CompositorShortcut)
            }
            WL_KEY_RELEASED => {
                ctx.guest_key_owner(host_keyboard_id, key)
                    == Some(GuestKeyOwner::CompositorShortcut)
            }
            _ => false,
        };

        let decision = match state {
            WL_KEY_PRESSED | WL_KEY_REPEATED => {
                let repeated = state == WL_KEY_REPEATED;
                if key != EVDEV_KEY_BACKSPACE {
                    Self::cancel_backspace_repeat(ctx, host_keyboard_id);
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
                let host_accelerator = !compositor_shortcut
                    && self.is_host_accelerator(host_keyboard_id, &ctx.accelerators, key);
                if host_accelerator {
                    // A host accelerator can also be visible through peek_key.
                    // It must never become a later IME recovery candidate.
                    Self::invalidate_peek_key_press(ctx, host_keyboard_id, key);
                }
                if compositor_shortcut {
                    // A Sommelier-owned shortcut is not guest input and must
                    // not become an IME recovery candidate if peek_key raced
                    // ahead of the regular keyboard event.
                    Self::invalidate_peek_key_press(ctx, host_keyboard_id, key);
                }
                if compositor_shortcut {
                    ctx.transition_guest_key(
                        host_keyboard_id,
                        key,
                        GuestKeyEvent::CompositorShortcutPress,
                    )
                } else {
                    ctx.transition_guest_key(
                        host_keyboard_id,
                        key,
                        GuestKeyEvent::PhysicalPress {
                            repeated,
                            host_accelerator,
                            ime_repeat_active: backspace_repeat_active,
                        },
                    )
                }
            }
            WL_KEY_RELEASED => {
                let decision = if compositor_shortcut {
                    ctx.transition_guest_key(
                        host_keyboard_id,
                        key,
                        GuestKeyEvent::CompositorShortcutRelease,
                    )
                } else {
                    ctx.transition_guest_key(host_keyboard_id, key, GuestKeyEvent::PhysicalRelease)
                };
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
    /// # Modifier policy: DEPRESSED | LATCHED only (no LOCKED)
    /// We intentionally exclude `STATE_MODS_LOCKED` (Caps Lock, etc.) from the
    /// accelerator modifier check. The C sommelier reference makes the same
    /// choice: host accelerators are defined in terms of actively-pressed keys,
    /// not persistent lock state. A `<Shift>` accelerator would not match when
    /// Caps Lock is on but Shift is not held — this is consistent with how
    /// ChromeOS accelerator keys are documented and tested.
    /// If locked-modifier accelerators are ever needed, change `components` to
    /// `xkb::STATE_MODS_EFFECTIVE` (which ORs depressed, latched, and locked).
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
        if let Some(state) = self.states.get_mut(&host_keyboard_id) {
            state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);

            let mut modifiers = 0;
            let components = xkb::STATE_MODS_DEPRESSED | xkb::STATE_MODS_LATCHED;
            // Use the xkbcommon logical modifier name constants (e.g. MOD_NAME_ALT = "Mod1")
            // rather than raw X11 modifier group strings. These are the stable canonical names
            // that match across different keyboard layouts, matching C sommelier's use of
            // XKB_MOD_NAME_ALT, XKB_MOD_NAME_LOGO, etc.
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
            self.modifiers.insert(host_keyboard_id, modifiers);
        } else {
            self.modifiers.remove(&host_keyboard_id);
            // XKB state is not yet initialised (keymap not yet received). The
            // Wayland spec permits modifiers to arrive before the keymap on
            // reconnect. The modifier bitmask stays at 0 (no modifiers assumed)
            // until the keymap arrives and on_modifiers is called again.
            log::debug!(
                "on_modifiers: XKB state not yet initialised (keymap not received); \
                 modifier event ignored (depressed={:#x}, latched={:#x}, locked={:#x})",
                mods_depressed,
                mods_latched,
                mods_locked
            );
        }
        Action::Forward
    }

    /// Clean up when the guest destroys the wl_keyboard object.
    ///
    /// `on_release` is a **client→host request**: `ctx.last_sender_id` is the
    /// **guest** keyboard ID. We must translate it to a host ID before looking
    /// up `keyboard_to_extended_keyboard` (which is keyed by host IDs).
    /// Using the typed [`GuestId`] / [`HostId`] wrappers makes a wrong-direction
    /// lookup a compile error.
    fn on_release(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        log::info!(">>> wl_keyboard.on_release: guest_id={}", guest_id);
        let guest_seat = ctx.keyboard_to_seat.remove(&guest_id);
        // Translate guest ID → host ID. Returns None for unknown keyboards
        // (e.g. keyboards that never received an on_enter event).
        let Some(host_keyboard_id) = ctx
            .shadow_table
            .host_id_of(GuestId::from_request_sender(ctx))
        else {
            if let Some(guest_seat) = guest_seat {
                crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
            }
            return Action::Forward;
        };
        let focus_update = ctx.keyboard_focus.release(host_keyboard_id);
        // Clear per-keyboard keymap, dropped keys, XKB state, and modifiers.
        // All must be reset so that a re-created keyboard starts from a clean
        // slate and doesn't inherit stale state from the previous session.
        //
        // accelerator suppression: a key dropped under the old session would
        //   cause its release event to be silently swallowed on the new keyboard.
        // modifiers: if the new keyboard receives a key event before the first
        //   wl_keyboard.modifiers, the accelerator check would use stale modifier
        //   bits and could produce wrong NOT_HANDLED/HANDLED decisions.
        self.clear_host_keyboard_keymap_and_state(ctx, host_keyboard_id);
        if let Some(host_extended_id) = ctx.keyboard_to_extended_keyboard.remove(&host_keyboard_id)
        {
            ctx.extended_keyboard_to_keyboard.remove(&host_extended_id);
            // zcr_extended_keyboard_v1.destroy — no payload (8-byte header only).
            let msg = crate::wire::MessageBuilder::new()
                .build_message(host_extended_id.0, ZCR_EXTENDED_KEYBOARD_DESTROY);
            ctx.client_to_host_queue.push((msg, Vec::new()));
            // Unregister from the host dispatch table so stale peek_key events
            // (version ≥ 2) sent after destroy cannot be dispatched to a dead object.
            ctx.shadow_table
                .mark_pending_destroy_host(host_extended_id.0);
            log::debug!(
                "Destroyed extended keyboard: host_extended_id={} for host_keyboard_id={}",
                host_extended_id.0,
                host_keyboard_id.0
            );
        }
        crate::handler::text_input::apply_keyboard_focus_changes(ctx, &focus_update.seat_changes);
        Action::Forward
    }
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
            WL_KEY_PRESSED | WL_KEY_REPEATED
                if state == WL_KEY_PRESSED || pressed_before =>
            {
                if state == WL_KEY_PRESSED && !peek_pressed_before {
                    let already_dropped_as_accelerator = ctx
                        .key_generations
                        .host_accelerator_suppressed(host_keyboard_id, key);
                    let eligible = !already_dropped_as_accelerator
                        && !self.is_host_accelerator(host_keyboard_id, &ctx.accelerators, key);
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
                } else if state == WL_KEY_REPEATED
                    || (state == WL_KEY_PRESSED && peek_pressed_before)
                {
                    // Both protocol v10 repeated and legacy duplicate pressed
                    // events describe the existing generation.
                    ctx.key_generations
                        .refresh_peek(host_keyboard_id, key, serial, time);
                }
                Self::update_host_keyboard_key_state(ctx, host_keyboard_id, key, state, serial);
                if state == WL_KEY_PRESSED && key != EVDEV_KEY_BACKSPACE {
                    Self::cancel_backspace_repeat(ctx, host_keyboard_id);
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
            }
            WL_KEY_REPEATED => {
                log::warn!(
                    "peek_key: ignoring repeated key {} without a preceding press",
                    key
                );
            }
            WL_KEY_RELEASED => {
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
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler;
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::ZwpTextInputV1Handler;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::wire::WireMessage;
    use std::os::unix::io::AsRawFd;

    /// Helper: create an anonymous memfd, write the default keymap into it
    /// (with a trailing NUL byte as required by the Wayland spec), and call
    /// on_keymap so the handler builds its XKB state. Returns the keymap
    /// object so tests can look up keycodes.
    fn load_test_keymap(handler: &mut KeyboardHandler, ctx: &mut Context) -> xkb::Keymap {
        let dummy_ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &dummy_ctx,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let keymap_str = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

        // Use an anonymous memfd so we don't need the `tempfile` crate.
        // nix is already a dependency of the main crate.
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;
        let name = CString::new("sommelier-test-keymap").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::empty()).expect("memfd_create failed");
        nix::unistd::write(&fd, keymap_str.as_bytes()).expect("write keymap failed");
        // Write the NUL terminator required by the Wayland spec
        // (wl_keyboard.keymap.size always includes it). on_keymap's debug_assert
        // checks for this byte, so omitting it would fire in debug builds.
        nix::unistd::write(&fd, &[0u8]).expect("write NUL failed");

        // size = string bytes + 1 NUL terminator, matching what a real host sends.
        let size = keymap_str.len() as u32 + 1;
        handler.on_keymap(ctx, 1, fd.as_raw_fd(), size);
        assert!(
            handler
                .keymaps
                .contains_key(&HostId::from_event_sender(ctx)),
            "keymap should be loaded for the sending keyboard"
        );
        keymap
    }

    /// Find the evdev keycode for a given keysym in the keymap.
    ///
    /// Returns `None` if the keysym is not present. Call sites should use
    /// `.expect("<description>")` so that test-failure output names the keysym
    /// that was missing rather than showing a bare panic from inside this helper.
    ///
    /// # Note on shift-level lookup
    /// This helper uses `key_get_syms_by_level(..., level=0)` which returns the
    /// **unshifted** (base level) symbol for each key. This is intentional for
    /// the purposes of *finding* a keycode given a base keysym: tests using this
    /// helper should not involve shifted accelerators (e.g. `<Shift>A`), since
    /// such tests would need a different lookup strategy. The production code uses
    /// `key_get_one_sym` which correctly accounts for the active shift level at
    /// runtime.
    fn find_keycode(keymap: &xkb::Keymap, target: u32) -> Option<u32> {
        let min = keymap.min_keycode().raw();
        let max = keymap.max_keycode().raw();
        for k in min..=max {
            let syms = keymap.key_get_syms_by_level(k.into(), 0, 0);
            if syms.iter().any(|s| s.raw() == target) {
                // XKB keycodes = evdev keycode + 8 (XKB_KEYCODE_OFFSET in xkbcommon.h).
                // Wayland wl_keyboard.key uses evdev keycodes, so subtract 8.
                return Some(k - 8);
            }
        }
        None
    }

    /// Return one binding from the nine-grid fixture used by placement tests.
    fn test_shortcut(chord: &str) -> WindowShortcut {
        let accelerator = crate::accelerator::parse_accelerator(chord).expect("valid test chord");
        ShortcutConfig::test_nine_grid()
            .find(accelerator)
            .expect("test chord must be present in the nine-grid fixture")
    }

    fn add_active_text_input(ctx: &mut Context, guest_id: u32, guest_seat: u32, host_ext_id: u32) {
        let host_v1_id = guest_id + 100;
        ctx.shadow_table.map_id(guest_id, host_v1_id);
        ctx.text_inputs.insert(
            guest_id,
            crate::state::TextInputState {
                host_v1_id,
                host_ext_id: Some(host_ext_id),
                guest_seat,
                active_surface: Some(900),
                pending_enabled: true,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: Some(("가".to_string(), 3, 3)),
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: String::new(),
                guest_commit_serial: 1,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: crate::state::HostActivationState::Active,
            },
        );
    }

    fn map_keyboard(
        ctx: &mut Context,
        guest_keyboard_id: u32,
        host_keyboard_id: u32,
        host_extended_id: u32,
        guest_seat: u32,
    ) {
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.keyboard_to_seat.insert(guest_keyboard_id, guest_seat);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        // Most handler tests use Backspace without loading a full XKB map.
        // Tests for other repeatable keys load the real test keymap.
        ctx.keyboard_repeatable_keys
            .entry(HostId(host_keyboard_id))
            .or_default()
            .insert(EVDEV_KEY_BACKSPACE);
    }

    fn focus_keyboard(
        ctx: &mut Context,
        host_keyboard_id: u32,
        guest_seat: u32,
        guest_surface: u32,
    ) {
        let host_surface = ctx
            .shadow_table
            .get_host_id(guest_surface)
            .unwrap_or(guest_surface);
        ctx.keyboard_focus.set_for_test(
            HostId(host_keyboard_id),
            guest_seat,
            guest_surface,
            host_surface,
        );
    }

    fn message_sender(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u32 {
        u32::from_ne_bytes(message.0[0..4].try_into().unwrap())
    }

    fn message_opcode(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u16 {
        (u32::from_ne_bytes(message.0[4..8].try_into().unwrap()) & 0xffff) as u16
    }

    fn message_first_u32(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u32 {
        u32::from_ne_bytes(message.0[8..12].try_into().unwrap())
    }

    fn keyboard_event_payload(
        message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>),
    ) -> (u32, u32, u32, u32) {
        let payload = &message.0[8..];
        (
            u32::from_ne_bytes(payload[0..4].try_into().unwrap()),
            u32::from_ne_bytes(payload[4..8].try_into().unwrap()),
            u32::from_ne_bytes(payload[8..12].try_into().unwrap()),
            u32::from_ne_bytes(payload[12..16].try_into().unwrap()),
        )
    }

    #[test]
    fn configured_shortcuts_cover_nine_alt_keys() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        handler
            .modifiers
            .insert(HostId(5), crate::accelerator::ALT_MASK);

        for sym in [
            xkb::keysyms::KEY_q,
            xkb::keysyms::KEY_w,
            xkb::keysyms::KEY_e,
            xkb::keysyms::KEY_a,
            xkb::keysyms::KEY_s,
            xkb::keysyms::KEY_d,
            xkb::keysyms::KEY_z,
            xkb::keysyms::KEY_x,
            xkb::keysyms::KEY_c,
        ] {
            let key = find_keycode(&keymap, sym).expect("layout keysym not found");
            assert!(
                handler
                    .window_shortcut(HostId(5), key, &ctx.shortcut_config.snapshot())
                    .is_some(),
                "configured nine-grid shortcut should match keysym {sym:#x}"
            );
        }
    }

    #[test]
    fn configured_shortcuts_require_exact_alt_modifier() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let key = find_keycode(&keymap, xkb::keysyms::KEY_q).expect("KEY_q not found");
        let config = ctx.shortcut_config.snapshot();

        handler.modifiers.insert(HostId(5), 0);
        assert_eq!(handler.window_shortcut(HostId(5), key, &config), None);
        handler.modifiers.insert(
            HostId(5),
            crate::accelerator::ALT_MASK | crate::accelerator::SHIFT_MASK,
        );
        assert_eq!(handler.window_shortcut(HostId(5), key, &config), None);
    }

    #[test]
    fn configured_shortcut_is_consumed_by_key_path_and_balanced_on_release() {
        let keyboard = 10u32;
        let host_keyboard = 5u32;
        let seat = 11u32;
        let surface = 12u32;
        let host_surface = 22u32;
        let xdg_toplevel = 13u32;
        let host_xdg_toplevel = 23u32;
        let output = 25u32;
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            ));
        map_keyboard(&mut ctx, keyboard, host_keyboard, 50, seat);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table.map_id(xdg_toplevel, host_xdg_toplevel);
        ctx.xdg_toplevel_to_wl_surface.insert(xdg_toplevel, surface);
        focus_keyboard(&mut ctx, host_keyboard, seat, surface);
        ctx.host_zaura_shell_id = Some(24);
        ctx.host_zaura_shell_version = 38;
        ctx.output_host_ids.push(output);
        ctx.output_states.insert(
            output,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                scale: 1,
                ..Default::default()
            },
        );
        ctx.last_sender_id = host_keyboard;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        handler
            .modifiers
            .insert(HostId(host_keyboard), crate::accelerator::ALT_MASK);
        let key = find_keycode(&keymap, xkb::keysyms::KEY_q).expect("KEY_q not found");

        assert_eq!(
            handler.on_key(&mut ctx, 1, 0, key, WL_KEY_PRESSED),
            Action::Drop
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard), key),
            Some(GuestKeyOwner::CompositorShortcut)
        );
        assert!(
            ctx.client_to_host_queue.iter().any(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                (word2 & 0xffff) as u16 == REQ_SET_WINDOW_BOUNDS
            }),
            "configured press must queue a direct bounds request"
        );

        assert_eq!(
            handler.on_key(&mut ctx, 2, 0, key, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(ctx.guest_key_owner(HostId(host_keyboard), key), None);
    }

    #[test]
    fn window_layout_is_disabled_without_geometry_method() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        assert!(!KeyboardHandler::apply_window_layout(
            &mut ctx,
            HostId(5),
            test_shortcut("<Alt>s"),
        ));
    }

    #[test]
    fn normalized_rect_bounds_use_two_by_two_work_area_cells() {
        let work_area = (0, 0, 3840, 2160);
        assert_eq!(
            test_shortcut("<Alt>q").rect.to_bounds(work_area),
            Some((0, 0, 1920, 1080))
        );
        assert_eq!(
            test_shortcut("<Alt>w").rect.to_bounds(work_area),
            Some((0, 0, 3840, 1080))
        );
        assert_eq!(
            test_shortcut("<Alt>e").rect.to_bounds(work_area),
            Some((1920, 0, 1920, 1080))
        );
        assert_eq!(
            test_shortcut("<Alt>a").rect.to_bounds(work_area),
            Some((0, 0, 1920, 2160))
        );
        assert_eq!(
            test_shortcut("<Alt>s").rect.to_bounds(work_area),
            Some((0, 0, 3840, 2160))
        );
        assert_eq!(
            test_shortcut("<Alt>z").rect.to_bounds(work_area),
            Some((0, 1080, 1920, 1080))
        );
        assert_eq!(
            test_shortcut("<Alt>x").rect.to_bounds(work_area),
            Some((0, 1080, 3840, 1080))
        );
        assert_eq!(
            test_shortcut("<Alt>c").rect.to_bounds(work_area),
            Some((1920, 1080, 1920, 1080))
        );
        assert_eq!(
            test_shortcut("<Alt>d").rect.to_bounds(work_area),
            Some((1920, 0, 1920, 2160))
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
        assert_eq!(
            ctx.guest_key_owner(keyboard, key),
            Some(GuestKeyOwner::CompositorShortcut)
        );

        let release =
            ctx.transition_guest_key(keyboard, key, GuestKeyEvent::CompositorShortcutRelease);
        assert_eq!(release.delivery, GuestKeyDelivery::Drop);
        assert_eq!(release.ack_handled, Some(true));
        assert!(release.ends_repeat);
        assert_eq!(ctx.guest_key_owner(keyboard, key), None);
    }

    #[test]
    fn apply_window_layout_queues_bounds_after_state_reset() {
        let keyboard = 10u32;
        let host_keyboard = 5u32;
        let seat = 11u32;
        let surface = 12u32;
        let host_surface = 22u32;
        let xdg_toplevel = 13u32;
        let host_xdg_toplevel = 23u32;
        let output = 25u32;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        // The production geometry backend must win if both flags are present;
        // the state type resolves that precedence before handlers run.
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            ));
        map_keyboard(&mut ctx, keyboard, host_keyboard, 50, seat);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table.map_id(xdg_toplevel, host_xdg_toplevel);
        ctx.xdg_toplevel_to_wl_surface.insert(xdg_toplevel, surface);
        focus_keyboard(&mut ctx, host_keyboard, seat, surface);
        ctx.host_zaura_shell_id = Some(24);
        ctx.host_zaura_shell_version = 38;
        ctx.output_host_ids.push(output);
        ctx.output_states.insert(
            output,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                scale: 1,
                ..Default::default()
            },
        );

        assert!(KeyboardHandler::apply_window_layout(
            &mut ctx,
            HostId(host_keyboard),
            test_shortcut("<Alt>s"),
        ));
        let opcodes: Vec<u16> = ctx
            .client_to_host_queue
            .iter()
            .map(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                (word2 & 0xffff) as u16
            })
            .collect();
        assert_eq!(
            opcodes,
            vec![
                crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
                crate::protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES,
                crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE,
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
                REQ_SET_WINDOW_BOUNDS,
                crate::protocols::wayland::wl_display::REQ_SYNC,
            ]
        );
        let bounds = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                u32::from_ne_bytes(message[0..4].try_into().unwrap())
                    == ctx
                        .window_placement
                        .aura_toplevel_for_xdg_toplevel(xdg_toplevel)
                        .unwrap()
                    && (word2 & 0xffff) as u16 == REQ_SET_WINDOW_BOUNDS
            })
            .expect("window bounds request")
            .0
            .as_slice();
        assert_eq!(i32::from_ne_bytes(bounds[8..12].try_into().unwrap()), 0);
        assert_eq!(i32::from_ne_bytes(bounds[12..16].try_into().unwrap()), 0);
        assert_eq!(i32::from_ne_bytes(bounds[16..20].try_into().unwrap()), 3840);
        assert_eq!(i32::from_ne_bytes(bounds[20..24].try_into().unwrap()), 2160);
    }

    #[test]
    fn left_and_right_use_direct_bounds_instead_of_snap_requests() {
        let keyboard = 10u32;
        let host_keyboard = 5u32;
        let seat = 11u32;
        let surface = 12u32;
        let host_surface = 22u32;
        let xdg_toplevel = 13u32;
        let host_xdg_toplevel = 23u32;
        let output = 25u32;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            ));
        map_keyboard(&mut ctx, keyboard, host_keyboard, 50, seat);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table.map_id(xdg_toplevel, host_xdg_toplevel);
        ctx.xdg_toplevel_to_wl_surface.insert(xdg_toplevel, surface);
        focus_keyboard(&mut ctx, host_keyboard, seat, surface);
        ctx.host_zaura_shell_id = Some(24);
        ctx.host_zaura_shell_version = 38;
        ctx.output_host_ids.push(output);
        ctx.output_states.insert(
            output,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                scale: 1,
                ..Default::default()
            },
        );

        for (shortcut, expected_x) in [
            (test_shortcut("<Alt>a"), 0),
            (test_shortcut("<Alt>d"), 1920),
        ] {
            ctx.client_to_host_queue.clear();
            assert!(KeyboardHandler::apply_window_layout(
                &mut ctx,
                HostId(host_keyboard),
                shortcut,
            ));
            let opcodes: Vec<u16> = ctx
                .client_to_host_queue
                .iter()
                .map(|(message, _)| {
                    let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                    (word2 & 0xffff) as u16
                })
                .collect();
            assert!(
                opcodes.contains(&REQ_SET_WINDOW_BOUNDS),
                "{shortcut:?} must use set_window_bounds"
            );
            assert!(
                !opcodes.contains(&crate::protocols::aura_shell::zaura_surface::REQ_SET_SNAP_LEFT),
                "{shortcut:?} must not use set_snap_left"
            );
            assert!(
                !opcodes.contains(&crate::protocols::aura_shell::zaura_surface::REQ_SET_SNAP_RIGHT),
                "{shortcut:?} must not use set_snap_right"
            );
            let bounds = ctx
                .client_to_host_queue
                .iter()
                .find(|(message, _)| {
                    let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                    u32::from_ne_bytes(message[0..4].try_into().unwrap())
                        == ctx
                            .window_placement
                            .aura_toplevel_for_xdg_toplevel(xdg_toplevel)
                            .unwrap()
                        && (word2 & 0xffff) as u16 == REQ_SET_WINDOW_BOUNDS
                })
                .expect("window bounds request")
                .0
                .as_slice();
            assert_eq!(
                i32::from_ne_bytes(bounds[8..12].try_into().unwrap()),
                expected_x
            );
            assert_eq!(i32::from_ne_bytes(bounds[12..16].try_into().unwrap()), 0);
            assert_eq!(i32::from_ne_bytes(bounds[16..20].try_into().unwrap()), 1920);
            assert_eq!(i32::from_ne_bytes(bounds[20..24].try_into().unwrap()), 2160);
        }
    }

    #[test]
    fn self_parent_probe_queues_self_parent_without_bounds_request() {
        let keyboard = 10u32;
        let host_keyboard = 5u32;
        let seat = 11u32;
        let surface = 12u32;
        let host_surface = 22u32;
        let xdg_toplevel = 13u32;
        let host_xdg_toplevel = 23u32;
        let output = 25u32;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Guest,
                crate::state::WindowGeometryMethod::SelfParent,
            ));
        map_keyboard(&mut ctx, keyboard, host_keyboard, 50, seat);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table.map_id(xdg_toplevel, host_xdg_toplevel);
        ctx.xdg_toplevel_to_wl_surface.insert(xdg_toplevel, surface);
        focus_keyboard(&mut ctx, host_keyboard, seat, surface);
        ctx.host_zaura_shell_id = Some(24);
        ctx.host_zaura_shell_version = 38;
        ctx.output_host_ids.push(output);
        ctx.output_states.insert(
            output,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                scale: 1,
                ..Default::default()
            },
        );
        let zaura_toplevel_id =
            crate::handler::compositor::ensure_zaura_toplevel(&mut ctx, xdg_toplevel)
                .expect("host zaura toplevel mapping");
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));

        assert!(KeyboardHandler::apply_window_layout(
            &mut ctx,
            HostId(host_keyboard),
            test_shortcut("<Alt>q"),
        ));
        let parent_request = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                (word2 & 0xffff) as u16 == REQ_SET_PARENT
            })
            .expect("self-parent request");
        let zaura_surface_id = ctx
            .window_placement
            .aura_surface_for_wl_surface(host_surface)
            .expect("host zaura surface mapping");
        assert_eq!(
            u32::from_ne_bytes(parent_request.0[0..4].try_into().unwrap()),
            zaura_surface_id
        );
        assert_eq!(
            u32::from_ne_bytes(parent_request.0[8..12].try_into().unwrap()),
            zaura_surface_id
        );
        assert_eq!(
            i32::from_ne_bytes(parent_request.0[12..16].try_into().unwrap()),
            -100
        );
        assert_eq!(
            i32::from_ne_bytes(parent_request.0[16..20].try_into().unwrap()),
            -200
        );
        assert!(
            !ctx.client_to_host_queue.iter().any(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                u32::from_ne_bytes(message[0..4].try_into().unwrap()) == zaura_toplevel_id
                    && (word2 & 0xffff) as u16 == REQ_SET_WINDOW_BOUNDS
            }),
            "self-parent probe must not also send set_window_bounds"
        );
        assert_eq!(
            ctx.window_placement.origin(zaura_toplevel_id),
            Some((0, 0)),
            "a self-parent placement predicts the new contents origin"
        );
        assert_eq!(
            ctx.window_placement.pending_origin(zaura_toplevel_id),
            Some((0, 0)),
            "the requested origin remains pending until host confirmation"
        );
    }

    #[test]
    fn self_parent_consumes_shortcut_until_screen_origin_is_known() {
        let keyboard = 10u32;
        let host_keyboard = 5u32;
        let seat = 11u32;
        let surface = 12u32;
        let host_surface = 22u32;
        let xdg_toplevel = 13u32;
        let output = 25u32;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Guest,
                crate::state::WindowGeometryMethod::SelfParent,
            ));
        map_keyboard(&mut ctx, keyboard, host_keyboard, 50, seat);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table.map_id(xdg_toplevel, 23);
        ctx.xdg_toplevel_to_wl_surface.insert(xdg_toplevel, surface);
        focus_keyboard(&mut ctx, host_keyboard, seat, surface);
        ctx.host_zaura_shell_id = Some(24);
        ctx.host_zaura_shell_version = 38;
        ctx.output_host_ids.push(output);
        ctx.output_states.insert(
            output,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                scale: 1,
                ..Default::default()
            },
        );
        let _zaura_toplevel_id =
            crate::handler::compositor::ensure_zaura_toplevel(&mut ctx, xdg_toplevel)
                .expect("host zaura toplevel mapping");

        assert!(
            KeyboardHandler::apply_window_layout(
                &mut ctx,
                HostId(host_keyboard),
                test_shortcut("<Alt>q"),
            ),
            "an early accelerator must be consumed rather than forwarded"
        );
        assert!(
            !ctx.client_to_host_queue.iter().any(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                (word2 & 0xffff) as u16 == REQ_SET_PARENT
            }),
            "no parent request is safe before the initial screen origin"
        );
    }

    #[test]
    fn self_parent_probe_rebases_following_shortcuts_on_target_origin() {
        let keyboard = 10u32;
        let host_keyboard = 5u32;
        let seat = 11u32;
        let surface = 12u32;
        let host_surface = 22u32;
        let xdg_toplevel = 13u32;
        let output = 25u32;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Guest,
                crate::state::WindowGeometryMethod::SelfParent,
            ));
        map_keyboard(&mut ctx, keyboard, host_keyboard, 50, seat);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table.map_id(xdg_toplevel, 23);
        ctx.xdg_toplevel_to_wl_surface.insert(xdg_toplevel, surface);
        focus_keyboard(&mut ctx, host_keyboard, seat, surface);
        ctx.host_zaura_shell_id = Some(24);
        ctx.host_zaura_shell_version = 38;
        ctx.output_host_ids.push(output);
        ctx.output_states.insert(
            output,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                scale: 1,
                ..Default::default()
            },
        );
        let zaura_toplevel_id =
            crate::handler::compositor::ensure_zaura_toplevel(&mut ctx, xdg_toplevel)
                .expect("host zaura toplevel mapping");
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));

        assert!(KeyboardHandler::apply_window_layout(
            &mut ctx,
            HostId(host_keyboard),
            test_shortcut("<Alt>q"),
        ));
        assert!(KeyboardHandler::apply_window_layout(
            &mut ctx,
            HostId(host_keyboard),
            test_shortcut("<Alt>c"),
        ));

        let parent_requests: Vec<_> = ctx
            .client_to_host_queue
            .iter()
            .filter(|(message, _)| {
                let word2 = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                (word2 & 0xffff) as u16 == REQ_SET_PARENT
            })
            .collect();
        assert_eq!(parent_requests.len(), 2);
        assert_eq!(
            i32::from_ne_bytes(parent_requests[0].0[12..16].try_into().unwrap()),
            -100
        );
        assert_eq!(
            i32::from_ne_bytes(parent_requests[0].0[16..20].try_into().unwrap()),
            -200
        );
        // The second request must be relative to the predicted (0, 0)
        // contents origin, not the stale (100, 200) origin.
        assert_eq!(
            i32::from_ne_bytes(parent_requests[1].0[12..16].try_into().unwrap()),
            1920
        );
        assert_eq!(
            i32::from_ne_bytes(parent_requests[1].0[16..20].try_into().unwrap()),
            1080
        );
        assert_eq!(
            ctx.window_placement.origin(zaura_toplevel_id),
            Some((1920, 1080))
        );
    }

    #[test]
    fn accelerator_keys_are_dropped_and_acked_not_handled() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );

        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");

        // Press Ctrl
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);

        // Set up extended keyboard tracking so ack_key is sent.
        // host_keyboard_extension_id must be Some to enable the protocol path.
        // Use non-reserved IDs (not 0 or 1, which are null/wl_display).
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));
        ctx.last_sender_id = 5;

        // Ctrl+A should be dropped (host accelerator)
        ctx.client_to_host_queue.clear();
        let action = handler.on_key(&mut ctx, 42, 0, wl_key_a, 1);
        assert_eq!(action, Action::Drop, "accelerator key should be dropped");

        // Verify ack_key was sent with handled=NOT_HANDLED (0)
        assert!(
            !ctx.client_to_host_queue.is_empty(),
            "ack_key should be queued"
        );
        let (msg, _) = &ctx.client_to_host_queue[0];
        // Message: [sender_id(4)] [size_opcode(4)] [serial(4)] [handled(4)]
        let handled_val = u32::from_ne_bytes(msg[12..16].try_into().unwrap());
        assert_eq!(handled_val, 0, "accelerator should be acked as NOT_HANDLED");

        // ChromiumOS also acknowledges the unmatched release as
        // NOT_HANDLED, and does not forward it to the guest.
        assert_eq!(
            handler.on_key(&mut ctx, 43, 1, wl_key_a, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        let (release_ack, _) = &ctx.client_to_host_queue[1];
        assert_eq!(
            u32::from_ne_bytes(release_ack[12..16].try_into().unwrap()),
            0,
            "accelerator release should be acked as NOT_HANDLED"
        );
    }

    #[test]
    fn non_accelerator_keys_are_forwarded_and_acked_handled() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );

        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_b =
            find_keycode(&keymap, xkb::keysyms::KEY_b).expect("KEY_b not found in keymap");

        // Press Ctrl
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);

        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));
        ctx.last_sender_id = 5;

        // Ctrl+B is NOT in accelerators → forward to guest
        ctx.client_to_host_queue.clear();
        let action = handler.on_key(&mut ctx, 43, 0, wl_key_b, 1);
        assert_eq!(
            action,
            Action::Forward,
            "non-accelerator key should be forwarded"
        );

        // Verify ack_key was sent with handled=HANDLED (1)
        let (msg, _) = &ctx.client_to_host_queue[0];
        let handled_val = u32::from_ne_bytes(msg[12..16].try_into().unwrap());
        assert_eq!(handled_val, 1, "non-accelerator should be acked as HANDLED");

        assert_eq!(
            handler.on_key(&mut ctx, 44, 1, wl_key_b, WL_KEY_RELEASED),
            Action::Forward
        );
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        let (release_ack, _) = &ctx.client_to_host_queue[1];
        assert_eq!(
            u32::from_ne_bytes(release_ack[12..16].try_into().unwrap()),
            1,
            "forwarded release should be acked as HANDLED"
        );
    }

    #[test]
    fn dropped_key_release_is_also_dropped() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );

        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");

        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);

        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));
        ctx.last_sender_id = 5;

        // Press → Drop
        let action = handler.on_key(&mut ctx, 1, 0, wl_key_a, 1);
        assert_eq!(action, Action::Drop);

        // Release → also Drop (prevents stuck key in guest)
        let action = handler.on_key(&mut ctx, 2, 0, wl_key_a, 0);
        assert_eq!(action, Action::Drop);

        // Next press of same key after release should still be evaluated
        let action = handler.on_key(&mut ctx, 3, 0, wl_key_a, 1);
        assert_eq!(action, Action::Drop);
    }

    #[test]
    fn ime_consumed_release_is_dropped_and_acked_not_handled() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.last_sender_id = 5;
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));
        assert!(ctx.claim_guest_key(HostId(5), EVDEV_KEY_BACKSPACE, GuestKeyOwner::ImeRecovery));

        // The IME fallback already emitted a synthetic press/release pair.
        // The later physical release must not create a stray guest release.
        assert_eq!(
            handler.on_key(&mut ctx, 7, 100, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let (ack, _) = &ctx.client_to_host_queue[0];
        assert_eq!(
            u32::from_ne_bytes(ack[12..16].try_into().unwrap()),
            0,
            "IME-consumed release must be acked as NOT_HANDLED"
        );
        assert!(
            ctx.guest_key_owner(HostId(5), EVDEV_KEY_BACKSPACE) == Some(GuestKeyOwner::ImeRecovery),
            "recovery ownership must remain as a late-event tombstone"
        );

        assert_eq!(
            handler.on_key(&mut ctx, 8, 200, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Forward,
            "the next physical generation must retire the recovery tombstone"
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(5), EVDEV_KEY_BACKSPACE),
            Some(GuestKeyOwner::Physical)
        );
    }

    #[test]
    fn untracked_release_is_dropped_and_acked_not_handled() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.last_sender_id = 5;
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));

        assert_eq!(
            handler.on_key(&mut ctx, 8, 100, 30, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let (ack, _) = &ctx.client_to_host_queue[0];
        assert_eq!(
            u32::from_ne_bytes(ack[12..16].try_into().unwrap()),
            0,
            "an unmatched release must not claim guest handling"
        );
    }

    #[test]
    fn duplicate_forwarded_press_is_dropped_but_release_remains_paired() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.last_sender_id = 5;
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));

        assert_eq!(
            handler.on_key(&mut ctx, 1, 100, 30, WL_KEY_PRESSED),
            Action::Forward
        );
        assert_eq!(
            handler.on_key(&mut ctx, 2, 101, 30, WL_KEY_PRESSED),
            Action::Drop,
            "duplicate press must not be sent twice to the guest"
        );
        assert_eq!(
            handler.on_key(&mut ctx, 3, 102, 30, WL_KEY_RELEASED),
            Action::Forward,
            "release must pair with the first forwarded press"
        );

        assert_eq!(ctx.client_to_host_queue.len(), 3);
        for (msg, _) in &ctx.client_to_host_queue {
            assert_eq!(
                u32::from_ne_bytes(msg[12..16].try_into().unwrap()),
                1,
                "duplicate and release events remain HANDLED by the guest"
            );
        }
    }

    #[test]
    fn repeat_without_press_does_not_create_key_state_or_claim_handled() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.last_sender_id = 5;
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));

        assert_eq!(
            handler.on_key(&mut ctx, 1, 100, 30, WL_KEY_REPEATED),
            Action::Drop,
            "a repeat without a forwarded press must not reach the guest"
        );
        assert!(
            !ctx.key_generations.physically_held(HostId(5), 30),
            "a repeat-only event must not create physical pressed-key state"
        );
        assert!(
            ctx.guest_key_owner(HostId(5), 30).is_none(),
            "a repeat-only event must not create forwarded-key state"
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let (ack, _) = &ctx.client_to_host_queue[0];
        assert_eq!(
            u32::from_ne_bytes(ack[12..16].try_into().unwrap()),
            0,
            "a malformed repeat must be acked as NOT_HANDLED"
        );
    }

    #[test]
    fn press_repeat_release_keeps_key_state_and_ack_balanced() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.last_sender_id = 5;
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));

        assert_eq!(
            handler.on_key(&mut ctx, 1, 100, 30, WL_KEY_PRESSED),
            Action::Forward
        );
        assert_eq!(
            handler.on_key(&mut ctx, 2, 101, 30, WL_KEY_REPEATED),
            Action::Forward
        );
        assert!(
            ctx.key_generations.physically_held(HostId(5), 30),
            "a valid repeat must preserve physical pressed-key state"
        );
        assert!(
            ctx.guest_key_owner(HostId(5), 30) == Some(GuestKeyOwner::Physical),
            "a valid repeat must preserve the outstanding forwarded press"
        );

        assert_eq!(
            handler.on_key(&mut ctx, 3, 102, 30, WL_KEY_RELEASED),
            Action::Forward
        );
        assert!(
            !ctx.key_generations.physically_held(HostId(5), 30),
            "release must retire physical pressed-key state"
        );
        assert!(
            ctx.guest_key_owner(HostId(5), 30).is_none(),
            "release must retire the outstanding forwarded press"
        );
        assert_eq!(ctx.client_to_host_queue.len(), 3);
        for (ack, _) in &ctx.client_to_host_queue {
            assert_eq!(
                u32::from_ne_bytes(ack[12..16].try_into().unwrap()),
                1,
                "each event in a valid press/repeat/release sequence is HANDLED"
            );
        }
    }

    /// Regression test: on_keymap must use mmap rather than read/seek because:
    /// 1. The host sends keymap data via shared memory (memfd); mmap reads from
    ///    offset 0 regardless of the fd cursor position.
    /// 2. read() would advance the fd offset, preventing the fd from being
    ///    forwarded with its original data intact.
    /// 3. On non-seekable fds the cursor cannot be reset to 0 at all.
    ///
    /// This test creates a memfd, writes the keymap, and does NOT seek back
    /// to the start — simulating a host that wrote and then sent the fd.
    /// The on_keymap implementation must still load the keymap via mmap.
    #[test]
    fn keymap_loads_from_non_rewound_memfd() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);

        let dummy_ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &dummy_ctx,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let keymap_str = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let keymap_bytes = keymap_str.as_bytes();

        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let name = CString::new("test-keymap-norw").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::empty()).expect("memfd_create failed");
        nix::unistd::write(&fd, keymap_bytes).expect("write failed");
        // Write the NUL terminator that on_keymap expects (wl_keyboard.keymap.size
        // always includes it). Without this, the mmap covers one byte beyond what
        // was written; we'd rely on OS zero-fill of fresh anonymous pages, which is
        // guaranteed on Linux but is not required by POSIX.
        nix::unistd::write(&fd, &[0u8]).expect("write NUL failed");
        // Deliberately do NOT seek back to 0.
        // read()/pread() from here would get 0 bytes or fail.
        // mmap with offset 0 must still work.

        handler.on_keymap(&mut ctx, 1, fd.as_raw_fd(), keymap_bytes.len() as u32 + 1);
        assert!(
            handler.keymaps.contains_key(&HostId(0)),
            "keymap must load via mmap even when fd cursor is at EOF"
        );
        assert!(
            handler.states.contains_key(&HostId(0)),
            "XKB state must be initialized after keymap load"
        );
        assert!(
            ctx.keyboard_keysym_to_keycode
                .get(&HostId(0))
                .is_some_and(|mapping| mapping.contains_key(&xkb::keysyms::KEY_A)),
            "the negotiated keymap must provide a keysym fallback map"
        );
        assert!(
            ctx.keyboard_keysym_to_keycode[&HostId(0)][&xkb::keysyms::KEY_a]
                == find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a must be mapped"),
            "the map must expose the same evdev keycode as the negotiated XKB keymap"
        );
    }

    #[test]
    fn mmap_view_rejects_negative_fd_before_borrowing() {
        assert!(
            MmapView::from_fd(-1, 1).is_none(),
            "negative descriptors must be rejected before BorrowedFd construction"
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
        let view = MmapView::from_fd(fd.as_raw_fd(), 4096).expect("mmap failed");
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

    #[test]
    fn on_release_destroys_extended_keyboard_and_cleans_map() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);

        // Use DISTINCT guest (5) and host (10) IDs to verify that on_release
        // correctly translates the guest sender ID to the host key, rather than
        // using last_sender_id (a guest ID) directly as a host map key.
        let guest_keyboard_id: u32 = 5;
        let host_keyboard_id: u32 = 10;
        let host_extended_id: u32 = 50;

        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 7);
        ctx.key_generations
            .record_latest_peek(7, None, HostId(host_keyboard_id), 1);
        ctx.key_generations
            .record_latest_peek(7, Some(100), HostId(host_keyboard_id), 2);
        ctx.key_generations
            .record_latest_peek(8, Some(200), HostId(11), 3);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        assert!(ctx
            .key_generations
            .claim_text_input_owner(HostId(host_keyboard_id), 30, 1));
        ctx.key_generations
            .observe_physical_state(HostId(host_keyboard_id), 30, WL_KEY_RELEASED);
        ctx.key_generations
            .observe_physical_state(HostId(host_keyboard_id), 30, WL_KEY_PRESSED);
        // Simulate a wl_keyboard.release from the guest (last_sender_id = guest ID).
        ctx.last_sender_id = guest_keyboard_id;

        let action = handler.on_release(&mut ctx);
        assert_eq!(action, Action::Forward);

        // Map entry must be removed so re-binding is possible.
        assert!(
            !ctx.keyboard_to_extended_keyboard
                .contains_key(&HostId(host_keyboard_id)),
            "extended keyboard map must be cleared after release"
        );
        assert!(
            !ctx.extended_keyboard_to_keyboard
                .contains_key(&HostId(host_extended_id)),
            "reverse extended keyboard map must be cleared after release"
        );
        assert!(
            !ctx.keyboard_to_seat.contains_key(&guest_keyboard_id),
            "guest keyboard-to-seat routing must be cleared after release"
        );
        assert!(
            !ctx.key_generations
                .take_pending_text_input_release(HostId(host_keyboard_id), 30, 2),
            "keyboard release must discard retired guest releases"
        );
        assert!(
            ctx.key_generations.latest_peek_sequence(7, None).is_none()
                && ctx
                    .key_generations
                    .latest_peek_sequence(7, Some(100))
                    .is_none(),
            "releasing a keyboard must retire the peek watermarks it owns"
        );
        assert_eq!(
            ctx.key_generations.latest_peek_sequence(8, Some(200)),
            Some(3),
            "another seat's watermark must remain intact"
        );

        // destroy message must have been queued to the host.
        assert_eq!(ctx.client_to_host_queue.len(), 1, "destroy must be queued");
        let (msg, _) = &ctx.client_to_host_queue[0];
        // Message: [sender_id(4)] [size_opcode(4)]  — opcode 0, len 8.
        let sender = u32::from_ne_bytes(msg[0..4].try_into().unwrap());
        let word2 = u32::from_ne_bytes(msg[4..8].try_into().unwrap());
        assert_eq!(
            sender, host_extended_id,
            "destroy must target host_extended_id"
        );
        assert_eq!(word2 >> 16, 8, "message length must be 8");
        assert_eq!(word2 & 0xFFFF, 0, "opcode must be 0 (destroy)");
    }

    #[test]
    fn bind_extended_keyboard_is_idempotent() {
        let mut ctx = Context::new(false, false);
        ctx.host_keyboard_extension_id = Some(HostId(99));

        // First call: should send get_extended_keyboard.
        KeyboardHandler::ensure_extended_keyboard_bound(&mut ctx, HostId(10));
        assert_eq!(ctx.client_to_host_queue.len(), 1);

        // Second call with the same host_keyboard_id: must not send again.
        ctx.client_to_host_queue.clear();
        KeyboardHandler::ensure_extended_keyboard_bound(&mut ctx, HostId(10));
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "bind must be idempotent: no second get_extended_keyboard"
        );
    }

    #[test]
    fn is_host_accelerator_returns_false_without_xkb_state() {
        let handler = KeyboardHandler::new(); // no keymap loaded
        let accelerators = crate::accelerator::parse_accelerators("<Control>a").unwrap();
        // Must degrade gracefully, not panic.
        assert!(
            !handler.is_host_accelerator(HostId(0), &accelerators, 30),
            "should return false when XKB state is not initialised"
        );
    }

    #[test]
    fn overflowing_evdev_keycode_is_not_an_accelerator() {
        let handler = KeyboardHandler::new();
        assert!(
            !handler.is_host_accelerator(
                HostId(0),
                &crate::accelerator::parse_accelerators("a").unwrap(),
                u32::MAX,
            ),
            "keycode + XKB offset must be checked before arithmetic"
        );
    }

    // -----------------------------------------------------------------------
    // New tests added by code review fixes
    // -----------------------------------------------------------------------

    #[test]
    fn on_keymap_ignores_zero_size() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        // A zero-size keymap must be rejected gracefully (mmap(len=0) is UB).
        // fd=0 (stdin) won't be mmap'd because the size check fires first.
        let action = handler.on_keymap(&mut ctx, 1 /* XKB_V1 */, 0, 0 /* size=0 */);
        assert_eq!(
            action,
            Action::Forward,
            "zero-size keymap must forward, not panic"
        );
        assert!(
            !handler.keymaps.contains_key(&HostId(0)),
            "keymap must not be set after zero-size event"
        );
    }

    #[test]
    fn no_keymap_format_leaves_no_keyboard_state() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        // Format 0 = WL_KEYMAP_FORMAT_NO_KEYMAP; forward without loading.
        let action = handler.on_keymap(&mut ctx, 0 /* format=no_keymap */, 0, 100);
        assert_eq!(action, Action::Forward);
        assert!(!handler.keymaps.contains_key(&HostId(0)));
    }

    #[test]
    fn unsupported_keymap_format_clears_previous_keyboard_state() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        load_test_keymap(&mut handler, &mut ctx);

        assert_eq!(handler.on_keymap(&mut ctx, 99, 0, 0), Action::Forward);
        assert!(!handler.keymaps.contains_key(&HostId(0)));
        assert!(!handler.states.contains_key(&HostId(0)));
    }

    #[test]
    fn keymap_without_trailing_nul_is_rejected_without_panicking() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        load_test_keymap(&mut handler, &mut ctx);

        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;
        let name = CString::new("test-keymap-missing-nul").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::empty()).expect("memfd_create failed");
        let malformed = b"xkb_keymap";
        nix::unistd::write(&fd, malformed).expect("write failed");

        assert_eq!(
            handler.on_keymap(
                &mut ctx,
                WL_KEYMAP_FORMAT_XKB_V1,
                fd.as_raw_fd(),
                malformed.len() as u32,
            ),
            Action::Forward
        );
        assert!(!handler.keymaps.contains_key(&HostId(0)));
        assert!(!handler.states.contains_key(&HostId(0)));
    }

    #[test]
    fn keymap_size_larger_than_fd_is_rejected_without_sigbus() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        load_test_keymap(&mut handler, &mut ctx);

        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;
        let name = CString::new("test-keymap-short-fd").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::empty()).expect("memfd_create failed");
        let bytes = b"\0";
        nix::unistd::write(&fd, bytes).expect("write failed");

        assert_eq!(
            handler.on_keymap(
                &mut ctx,
                WL_KEYMAP_FORMAT_XKB_V1,
                fd.as_raw_fd(),
                (bytes.len() + 1) as u32,
            ),
            Action::Forward
        );
        assert!(
            !handler.keymaps.contains_key(&HostId(0)),
            "a short keymap fd must not leave stale keymap state"
        );
        assert!(
            !handler.states.contains_key(&HostId(0)),
            "a short keymap fd must not leave stale XKB state"
        );
    }

    #[test]
    fn no_keymap_event_clears_previous_keyboard_state() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 1, ctrl_mask, 0, 0, 0);
        ctx.key_generations.suppress_host_accelerator(HostId(5), 30);

        assert_eq!(handler.on_keymap(&mut ctx, 0, 0, 0), Action::Forward);
        assert!(!handler.keymaps.contains_key(&HostId(5)));
        assert!(!handler.states.contains_key(&HostId(5)));
        assert!(!handler.modifiers.contains_key(&HostId(5)));
        assert!(!ctx
            .key_generations
            .host_accelerator_suppressed(HostId(5), 30));
    }

    #[test]
    fn no_keymap_event_does_not_clear_another_keyboard() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        ctx.last_sender_id = 5;
        load_test_keymap(&mut handler, &mut ctx);
        ctx.last_sender_id = 6;
        load_test_keymap(&mut handler, &mut ctx);

        ctx.last_sender_id = 5;
        assert_eq!(handler.on_keymap(&mut ctx, 0, 0, 0), Action::Forward);
        assert!(!handler.keymaps.contains_key(&HostId(5)));
        assert!(!handler.states.contains_key(&HostId(5)));
        assert!(
            handler.keymaps.contains_key(&HostId(6)),
            "one keyboard's NO_KEYMAP event must preserve another keyboard's keymap"
        );
        assert!(
            handler.states.contains_key(&HostId(6)),
            "one keyboard's NO_KEYMAP event must preserve another keyboard's XKB state"
        );
    }

    /// Regression: on_keymap must log an error and not crash when the
    /// keymap data is not valid UTF-8, or when xkbcommon rejects the
    /// string. In both cases the handler must degrade gracefully:
    /// keymap stays absent, state stays absent, Action::Forward is returned.
    #[test]
    fn on_keymap_logs_error_on_invalid_utf8() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);

        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;
        let name = CString::new("test-bad-keymap").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::empty()).expect("memfd_create failed");
        // Write a lone continuation byte — invalid UTF-8 — followed by a NUL
        // terminator, because wl_keyboard.keymap.size always includes the NUL.
        let bad_bytes: &[u8] = &[0xFF, 0xFE, 0xFD, 0x00];
        nix::unistd::write(&fd, bad_bytes).expect("write failed");

        use std::os::unix::io::AsRawFd;
        let action = handler.on_keymap(
            &mut ctx,
            1, /* XKB_V1 */
            fd.as_raw_fd(),
            bad_bytes.len() as u32,
        );
        assert_eq!(action, Action::Forward, "invalid UTF-8 must still forward");
        assert!(
            !handler.keymaps.contains_key(&HostId(0)),
            "keymap must remain absent on parse error"
        );
        assert!(
            !handler.states.contains_key(&HostId(0)),
            "state must remain absent on parse error"
        );
    }

    #[test]
    fn on_keymap_logs_error_on_invalid_xkb_string() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);

        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;
        let name = CString::new("test-bad-xkb").unwrap();
        let fd = memfd_create(name.as_c_str(), MFdFlags::empty()).expect("memfd_create failed");
        // Valid UTF-8 but not a valid XKB keymap. Include trailing NUL because
        // wl_keyboard.keymap.size always includes the NUL terminator.
        let garbage = b"this is not a valid xkb keymap\0";
        nix::unistd::write(&fd, garbage).expect("write failed");

        use std::os::unix::io::AsRawFd;
        let action = handler.on_keymap(
            &mut ctx,
            1, /* XKB_V1 */
            fd.as_raw_fd(),
            garbage.len() as u32,
        );
        assert_eq!(
            action,
            Action::Forward,
            "invalid XKB string must still forward"
        );
        assert!(
            !handler.keymaps.contains_key(&HostId(0)),
            "keymap must remain absent on XKB compile error"
        );
        assert!(
            !handler.states.contains_key(&HostId(0)),
            "state must remain absent on XKB compile error"
        );
    }

    #[test]
    fn send_ack_key_warns_when_protocol_bound_but_keyboard_not_registered() {
        // Regression: if host_keyboard_extension_id is Some (protocol available)
        // but the keyboard hasn't been registered via on_enter yet, send_ack_key
        // must not panic or silently send a garbage ack. No queue entry expected.
        let mut ctx = Context::new(false, false);
        ctx.host_keyboard_extension_id = Some(HostId(99)); // protocol bound
                                                           // keyboard_to_extended_keyboard is empty (on_enter not yet received)

        KeyboardHandler::send_ack_key(&mut ctx, HostId(10), 1, true);

        assert!(
            ctx.client_to_host_queue.is_empty(),
            "no ack_key must be queued when keyboard is not yet registered"
        );
    }

    #[test]
    fn send_ack_key_keeps_existing_child_alive_after_manager_global_remove() {
        let mut ctx = Context::new(false, false);
        // The manager global may disappear after the child was created. The
        // child mapping and dispatch object have an independent lifetime, so
        // ack_key must still be routed even though the manager field is gone.
        ctx.host_keyboard_extension_id = None;
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(10), HostId(50));

        KeyboardHandler::send_ack_key(&mut ctx, HostId(10), 7, true);

        assert_eq!(
            ctx.client_to_host_queue.len(),
            1,
            "an existing extended-keyboard child must still receive ack_key"
        );
        let (message, _) = &ctx.client_to_host_queue[0];
        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()),
            50,
            "ack_key must target the child object, not the retired manager"
        );
    }

    #[test]
    fn peek_key_tracks_ime_consumed_physical_key_state() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);

        ctx.last_sender_id = 1000;
        assert_eq!(handler.on_peek_key(&mut ctx, 10, 20, 14, 1), Action::Drop);
        assert!(
            ctx.key_generations.physically_held(HostId(100), 14),
            "peek press must be scoped to its host keyboard"
        );

        assert_eq!(handler.on_peek_key(&mut ctx, 11, 21, 14, 0), Action::Drop);
        assert!(!ctx.key_generations.physically_held(HostId(100), 14));
    }

    #[test]
    fn repeated_peek_key_keeps_the_physical_key_down_without_rearming_ime_cancel() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);

        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, 10, 20, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        ctx.key_generations
            .cancel_backspace_repeat(HostId(100), EVDEV_KEY_BACKSPACE);

        assert_eq!(
            handler.on_peek_key(&mut ctx, 11, 21, EVDEV_KEY_BACKSPACE, WL_KEY_REPEATED,),
            Action::Drop
        );
        assert!(
            ctx.key_generations
                .physically_held(HostId(100), EVDEV_KEY_BACKSPACE),
            "repeated peek must preserve the physical pressed-key state"
        );
        assert!(
            ctx.key_generations
                .backspace_repeat_cancelled(HostId(100), EVDEV_KEY_BACKSPACE),
            "a repeated peek must not re-arm a cancelled IME fallback"
        );
    }

    #[test]
    fn repeated_peek_key_without_press_does_not_create_physical_state() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);

        ctx.last_sender_id = 1000;
        assert_eq!(
            handler.on_peek_key(&mut ctx, 11, 21, EVDEV_KEY_BACKSPACE, WL_KEY_REPEATED,),
            Action::Drop
        );
        assert!(!ctx
            .key_generations
            .physically_held(HostId(100), EVDEV_KEY_BACKSPACE));
        assert!(ctx
            .key_generations
            .peek(HostId(100), EVDEV_KEY_BACKSPACE)
            .is_none());
    }

    #[test]
    fn backspace_peek_identity_survives_unrelated_release() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);

        ctx.last_sender_id = 1000;
        assert_eq!(
            handler.on_peek_key(&mut ctx, 20, 200, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Drop
        );

        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 21, 210, 30, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(
            ctx.key_generations
                .peek(HostId(100), EVDEV_KEY_BACKSPACE)
                .map(|press| (press.serial, press.time)),
            Some((20, 200)),
            "an unrelated key event must not replace the Backspace peek identity"
        );
    }

    #[test]
    fn ime_repeat_state_is_scoped_to_the_originating_keyboard() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        map_keyboard(&mut ctx, 11, 101, 1001, 2);
        add_active_text_input(&mut ctx, 40, 1, 2000);

        // The IME consumed Backspace on keyboard 100.
        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, 1, 500, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(100), EVDEV_KEY_BACKSPACE, 40));
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 500, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Drop
        );

        // A simultaneous Backspace on keyboard 101 must not be swallowed just
        // because keyboard 100 has an active IME repeat.
        ctx.last_sender_id = 101;
        assert_eq!(
            handler.on_key(&mut ctx, 2, 501, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Forward
        );
    }

    #[test]
    fn synthetic_ime_key_uses_the_host_compositor_time_domain() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);

        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, 1, 1234, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler::on_confirm_preedit(
                &mut crate::handler::text_input::ExtendedTextInputV1Handler,
                &mut ctx,
                0,
            ),
            Action::Drop
        );

        let payload = &ctx.host_to_client_queue[0].0[8..];
        let synthetic_time = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        assert_eq!(
            synthetic_time, 1234,
            "synthetic key events must use the host wl_keyboard time"
        );
    }

    #[test]
    fn equal_serial_peek_release_uses_timestamp_order() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        let keyboard = HostId(100);
        let key = 20;
        let shared_serial = 18_857;

        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, shared_serial, 100, key, WL_KEY_PRESSED);
        handler.on_peek_key(&mut ctx, shared_serial, 110, key, WL_KEY_RELEASED);
        assert!(
            !ctx.key_generations.physically_held(keyboard, key),
            "a later release must close a press even when ChromeOS reuses its serial"
        );

        handler.on_peek_key(&mut ctx, shared_serial, 130, key, WL_KEY_PRESSED);
        handler.on_peek_key(&mut ctx, shared_serial, 120, key, WL_KEY_RELEASED);
        assert!(
            ctx.key_generations.physically_held(keyboard, key),
            "an older delayed release must not close the current generation"
        );
        handler.on_peek_key(&mut ctx, shared_serial, 130, key, WL_KEY_RELEASED);
        assert!(
            !ctx.key_generations.physically_held(keyboard, key),
            "equal timestamps must remain valid for fast press/release pairs"
        );

        handler.on_peek_key(&mut ctx, shared_serial + 2, 150, key, WL_KEY_PRESSED);
        handler.on_peek_key(&mut ctx, shared_serial + 1, 160, key, WL_KEY_RELEASED);
        assert!(
            ctx.key_generations.physically_held(keyboard, key),
            "an older serial must stay stale even when delivered with a newer timestamp"
        );
    }

    #[test]
    fn reused_peek_serial_does_not_recover_stale_t_for_backspace() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let t = find_keycode(&keymap, xkb::keysyms::KEY_t).expect("T in keymap");
        let shared_serial = 18_857;

        // Captured ChromeOS behavior: distinct key transitions shared one
        // Wayland serial while their compositor timestamps kept increasing.
        // If either release remains latched, the next Backspace press refreshes
        // an old generation and empty IME confirmations recover T instead.
        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(
            &mut ctx,
            shared_serial,
            100,
            EVDEV_KEY_BACKSPACE,
            WL_KEY_PRESSED,
        );
        keyboard_handler.on_peek_key(
            &mut ctx,
            shared_serial,
            110,
            EVDEV_KEY_BACKSPACE,
            WL_KEY_RELEASED,
        );
        keyboard_handler.on_peek_key(&mut ctx, shared_serial, 120, t, WL_KEY_PRESSED);
        keyboard_handler.on_peek_key(&mut ctx, shared_serial, 130, t, WL_KEY_RELEASED);
        keyboard_handler.on_peek_key(
            &mut ctx,
            shared_serial,
            140,
            EVDEV_KEY_BACKSPACE,
            WL_KEY_PRESSED,
        );

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        let (_, _, recovered_key, state) = keyboard_event_payload(&ctx.host_to_client_queue[0]);
        assert_eq!(recovered_key, EVDEV_KEY_BACKSPACE);
        assert_eq!(state, WL_KEY_PRESSED);
    }

    #[test]
    fn held_space_repeats_after_ime_commit_until_physical_release() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        let guest_keyboard = 10;
        let host_keyboard = 100;
        let host_extended_keyboard = 1000;
        let guest_seat = 1;
        let guest_text_input = 40;
        let host_text_input = guest_text_input + 100;
        let host_extended_text_input = 2000;

        map_keyboard(
            &mut ctx,
            guest_keyboard,
            host_keyboard,
            host_extended_keyboard,
            guest_seat,
        );
        add_active_text_input(
            &mut ctx,
            guest_text_input,
            guest_seat,
            host_extended_text_input,
        );
        focus_keyboard(&mut ctx, host_keyboard, guest_seat, 900);

        ctx.last_sender_id = host_keyboard;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        assert!(ctx.keyboard_repeatable_keys[&HostId(host_keyboard)].contains(&space));

        // Captured ChromeOS sequence: the host IME consumes the physical Space
        // and includes the first one in its committed string. Further repeat
        // ticks arrive only as empty confirm_preedit events.
        ctx.last_sender_id = host_extended_keyboard;
        assert_eq!(
            keyboard_handler.on_peek_key(&mut ctx, 700, 1234, space, WL_KEY_PRESSED),
            Action::Drop
        );

        let mut text_input_handler = crate::handler::text_input::TextInputV1Handler;
        ctx.last_sender_id = host_text_input;
        assert_eq!(
            text_input_handler.on_preedit_string(&mut ctx, 1, &"가".to_string(), &String::new(),),
            Action::Drop
        );
        assert_eq!(
            text_input_handler.on_commit_string(&mut ctx, 1, &"가 ".to_string()),
            Action::Drop
        );

        let committed: Vec<_> = ctx
            .host_to_client_queue
            .iter()
            .filter(|message| {
                message_sender(message) == guest_text_input && message_opcode(message) == 3
            })
            .map(|message| {
                let mut wire = WireMessage::new(guest_text_input, 3, &message.0[8..], &message.1);
                wire.read_string().unwrap().to_string()
            })
            .collect();
        assert_eq!(committed, vec!["가 "], "the initial Space must commit once");

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = host_extended_text_input;
        let mut extended_handler = crate::handler::text_input::ExtendedTextInputV1Handler;
        for _ in 0..3 {
            assert_eq!(
                extended_handler.on_confirm_preedit(&mut ctx, 1),
                Action::Drop
            );
        }

        assert_eq!(ctx.host_to_client_queue.len(), 9);
        let mut serials = std::collections::HashSet::new();
        let (transactions, remainder) = ctx.host_to_client_queue.as_chunks::<3>();
        assert!(
            remainder.is_empty(),
            "synthetic key transactions must have three messages"
        );
        for transaction in transactions {
            for (message, expected_state) in transaction[..2]
                .iter()
                .zip([WL_KEY_PRESSED, WL_KEY_RELEASED])
            {
                assert_eq!(message_sender(message), guest_keyboard);
                assert_eq!(message_opcode(message), wl_keyboard::EVT_KEY);
                let (serial, time, key, state) = keyboard_event_payload(message);
                assert!(serials.insert(serial), "synthetic serials must be unique");
                assert_eq!(time, 1234);
                assert_eq!(key, space);
                assert_eq!(state, expected_state);
            }
            assert_eq!(message_sender(&transaction[2]), guest_text_input);
            assert_eq!(message_opcode(&transaction[2]), 5);
        }

        let delivered_before_release = ctx.host_to_client_queue.len();
        ctx.last_sender_id = host_extended_keyboard;
        assert_eq!(
            keyboard_handler.on_peek_key(&mut ctx, 701, 1300, space, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), delivered_before_release);
        assert!(!ctx
            .key_generations
            .physically_held(HostId(host_keyboard), space));
        assert!(ctx
            .key_generations
            .peek(HostId(host_keyboard), space)
            .is_none());
        assert!(
            ctx.guest_key_owner(HostId(host_keyboard), space) == Some(GuestKeyOwner::ImeRecovery)
        );

        ctx.last_sender_id = host_extended_text_input;
        assert_eq!(
            extended_handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            delivered_before_release,
            "a confirmation after physical release must be a no-op"
        );
    }

    #[test]
    fn delayed_keyboard_events_do_not_duplicate_an_ime_recovered_key() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );

        ctx.last_sender_id = 100;
        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED),
            Action::Drop
        );
        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 3, 502, space, WL_KEY_REPEATED),
            Action::Drop
        );
        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 4, 503, space, WL_KEY_RELEASED);
        ctx.last_sender_id = 100;
        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 4, 503, space, WL_KEY_RELEASED),
            Action::Drop
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );
        assert!(!ctx.key_generations.physically_held(HostId(100), space));
        assert!(ctx.key_generations.peek(HostId(100), space).is_none());
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );

        let handled: Vec<_> = ctx
            .client_to_host_queue
            .iter()
            .map(|message| u32::from_ne_bytes(message.0[12..16].try_into().unwrap()))
            .collect();
        assert_eq!(
            handled,
            vec![0, 0, 0],
            "recovered key events must remain host-owned"
        );
    }

    #[test]
    fn delayed_keysym_events_do_not_duplicate_an_ime_recovered_key() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        let recovered_messages = ctx.host_to_client_queue.len();

        ctx.last_sender_id = 140;
        let mut text_input_handler = crate::handler::text_input::TextInputV1Handler;
        assert_eq!(
            text_input_handler.on_keysym(
                &mut ctx,
                501,
                2,
                xkb::keysyms::KEY_space,
                WL_KEY_PRESSED,
                0,
            ),
            Action::Drop
        );
        assert_eq!(
            text_input_handler.on_keysym(
                &mut ctx,
                502,
                3,
                xkb::keysyms::KEY_space,
                WL_KEY_RELEASED,
                0,
            ),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), recovered_messages);
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 4, 503, space, WL_KEY_RELEASED);
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );
    }

    #[test]
    fn recovery_tombstone_suppresses_keysym_after_physical_release() {
        #[derive(Clone, Copy)]
        enum DelayedEvent {
            KeyboardPress,
            KeyboardRelease,
            PeekRelease,
            KeysymPress,
            KeysymRelease,
        }

        let orderings = [
            &[
                DelayedEvent::KeysymPress,
                DelayedEvent::KeysymRelease,
                DelayedEvent::KeyboardPress,
                DelayedEvent::KeyboardRelease,
                DelayedEvent::PeekRelease,
            ][..],
            &[
                DelayedEvent::KeyboardPress,
                DelayedEvent::KeysymPress,
                DelayedEvent::KeysymRelease,
                DelayedEvent::KeyboardRelease,
                DelayedEvent::PeekRelease,
            ],
            &[
                DelayedEvent::KeyboardPress,
                DelayedEvent::KeyboardRelease,
                DelayedEvent::KeysymPress,
                DelayedEvent::KeysymRelease,
                DelayedEvent::PeekRelease,
            ],
            &[
                DelayedEvent::KeysymPress,
                DelayedEvent::KeyboardPress,
                DelayedEvent::PeekRelease,
                DelayedEvent::KeysymRelease,
                DelayedEvent::KeyboardRelease,
            ],
            &[
                DelayedEvent::KeyboardPress,
                DelayedEvent::PeekRelease,
                DelayedEvent::KeysymPress,
                DelayedEvent::KeysymRelease,
                DelayedEvent::KeyboardRelease,
            ],
            &[
                DelayedEvent::KeysymPress,
                DelayedEvent::KeyboardPress,
                DelayedEvent::KeyboardRelease,
                DelayedEvent::PeekRelease,
                DelayedEvent::KeysymRelease,
            ],
            &[
                DelayedEvent::PeekRelease,
                DelayedEvent::KeyboardPress,
                DelayedEvent::KeyboardRelease,
                DelayedEvent::KeysymPress,
                DelayedEvent::KeysymRelease,
            ],
        ];

        for (ordering_index, ordering) in orderings.into_iter().enumerate() {
            let mut ctx = Context::new_for_test(false, false, Vec::new());
            let mut keyboard_handler = KeyboardHandler::new();
            let mut text_input_handler = crate::handler::text_input::TextInputV1Handler;
            map_keyboard(&mut ctx, 10, 100, 1000, 1);
            add_active_text_input(&mut ctx, 40, 1, 2000);
            ctx.last_sender_id = 100;
            let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
            let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

            ctx.last_sender_id = 1000;
            keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
            ctx.last_sender_id = 2000;
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1);
            let recovered_messages = ctx.host_to_client_queue.len();
            assert_eq!(recovered_messages, 3);

            for (event_index, event) in ordering.iter().enumerate() {
                match event {
                    DelayedEvent::KeyboardPress => {
                        ctx.last_sender_id = 100;
                        keyboard_handler.on_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
                    }
                    DelayedEvent::KeyboardRelease => {
                        ctx.last_sender_id = 100;
                        keyboard_handler.on_key(&mut ctx, 2, 503, space, WL_KEY_RELEASED);
                    }
                    DelayedEvent::PeekRelease => {
                        ctx.last_sender_id = 1000;
                        keyboard_handler.on_peek_key(&mut ctx, 2, 503, space, WL_KEY_RELEASED);
                    }
                    DelayedEvent::KeysymPress => {
                        ctx.last_sender_id = 140;
                        text_input_handler.on_keysym(
                            &mut ctx,
                            500,
                            1,
                            xkb::keysyms::KEY_space,
                            WL_KEY_PRESSED,
                            0,
                        );
                    }
                    DelayedEvent::KeysymRelease => {
                        ctx.last_sender_id = 140;
                        text_input_handler.on_keysym(
                            &mut ctx,
                            503,
                            2,
                            xkb::keysyms::KEY_space,
                            WL_KEY_RELEASED,
                            0,
                        );
                    }
                }
                assert_eq!(
                    ctx.host_to_client_queue.len(),
                    recovered_messages,
                    "ordering {ordering_index}, event {event_index} duplicated recovery"
                );
                assert_eq!(
                    ctx.guest_key_owner(HostId(100), space),
                    Some(GuestKeyOwner::ImeRecovery)
                );
            }

            // A new physical generation is the retirement boundary. Its
            // keysym pair must be delivered normally.
            ctx.last_sender_id = 1000;
            keyboard_handler.on_peek_key(&mut ctx, 6, 600, space, WL_KEY_PRESSED);
            assert!(ctx.guest_key_owner(HostId(100), space).is_none());
            ctx.last_sender_id = 140;
            text_input_handler.on_keysym(
                &mut ctx,
                601,
                7,
                xkb::keysyms::KEY_space,
                WL_KEY_PRESSED,
                0,
            );
            text_input_handler.on_keysym(
                &mut ctx,
                602,
                8,
                xkb::keysyms::KEY_space,
                WL_KEY_RELEASED,
                0,
            );
            assert_eq!(ctx.host_to_client_queue.len(), recovered_messages + 2);
            assert_eq!(
                ctx.guest_key_owner(HostId(100), space),
                Some(GuestKeyOwner::ImeRecovery)
            );
        }
    }

    #[test]
    fn stale_keyboard_press_cannot_steal_a_new_peek_generation() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
        ctx.last_sender_id = 2000;
        crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1);
        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 2, 510, space, WL_KEY_RELEASED);
        keyboard_handler.on_peek_key(&mut ctx, 10, 600, space, WL_KEY_PRESSED);
        assert!(ctx.guest_key_owner(HostId(100), space).is_none());

        ctx.last_sender_id = 100;
        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED),
            Action::Drop,
            "the prior generation's matching serial must not open the current key"
        );
        assert!(ctx.guest_key_owner(HostId(100), space).is_none());
        assert_eq!(
            ctx.key_generations.peek_press_serial(HostId(100), space),
            Some(10)
        );

        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 10, 600, space, WL_KEY_PRESSED),
            Action::Forward,
            "the current peek generation must still accept its matching key"
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::Physical)
        );
    }

    #[test]
    fn delayed_release_closes_only_its_retired_physical_generation() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, 1, 500, 30, WL_KEY_PRESSED);
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 500, 30, WL_KEY_PRESSED),
            Action::Forward
        );

        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, 2, 510, 30, WL_KEY_RELEASED);
        handler.on_peek_key(&mut ctx, 10, 600, 30, WL_KEY_PRESSED);
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 10, 600, 30, WL_KEY_PRESSED),
            Action::Forward
        );

        assert_eq!(
            handler.on_key(&mut ctx, 2, 510, 30, WL_KEY_RELEASED),
            Action::Forward,
            "the delayed release must close the retired guest press"
        );
        assert!(ctx.key_generations.physically_held(HostId(100), 30));
        assert_eq!(
            ctx.guest_key_owner(HostId(100), 30),
            Some(GuestKeyOwner::Physical),
            "the current generation must keep its owner"
        );

        ctx.last_sender_id = 1000;
        handler.on_peek_key(&mut ctx, 11, 610, 30, WL_KEY_RELEASED);
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 11, 610, 30, WL_KEY_RELEASED),
            Action::Forward
        );
        assert!(!ctx.key_generations.physically_held(HostId(100), 30));
        assert!(ctx.guest_key_owner(HostId(100), 30).is_none());
    }

    #[test]
    fn keysym_release_does_not_end_a_physical_ime_consumed_generation() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        focus_keyboard(&mut ctx, 100, 1, 900);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);

        // ChromeOS may deliver a text-input keysym pair for the initial key
        // while peek_key remains the sole source of the physical hold state.
        ctx.last_sender_id = 140;
        let mut text_input_handler = crate::handler::text_input::TextInputV1Handler;
        text_input_handler.on_keysym(&mut ctx, 501, 2, xkb::keysyms::KEY_space, WL_KEY_PRESSED, 0);
        text_input_handler.on_keysym(
            &mut ctx,
            502,
            3,
            xkb::keysyms::KEY_space,
            WL_KEY_RELEASED,
            0,
        );

        assert!(ctx.key_generations.physically_held(HostId(100), space));
        assert!(ctx.key_generations.peek(HostId(100), space).is_some());
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery),
            "the balanced keysym pair must suppress delayed physical channels"
        );

        let delivered_keysym_messages = ctx.host_to_client_queue.len();
        ctx.last_sender_id = 100;
        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), delivered_keysym_messages);
        let (ack, _) = ctx.client_to_host_queue.last().expect("delayed key ack");
        assert_eq!(
            u32::from_ne_bytes(ack[12..16].try_into().unwrap()),
            0,
            "a suppressed duplicate press must be acked as NOT_HANDLED"
        );

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            3,
            "an empty confirmation must recover one balanced key pair and done"
        );
        for (message, expected_state) in ctx.host_to_client_queue[..2]
            .iter()
            .zip([WL_KEY_PRESSED, WL_KEY_RELEASED])
        {
            let (_, _, recovered_key, state) = keyboard_event_payload(message);
            assert_eq!(recovered_key, space);
            assert_eq!(state, expected_state);
        }

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 4, 503, space, WL_KEY_RELEASED);
        assert!(!ctx.key_generations.physically_held(HostId(100), space));
        assert!(ctx.key_generations.peek(HostId(100), space).is_none());
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );
    }

    #[test]
    fn keysym_and_repeat_recovery_share_the_latest_keyboard_owner() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        map_keyboard(&mut ctx, 11, 101, 1001, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        focus_keyboard(&mut ctx, 100, 1, 900);
        focus_keyboard(&mut ctx, 101, 1, 900);

        ctx.last_sender_id = 100;
        load_test_keymap(&mut keyboard_handler, &mut ctx);
        ctx.last_sender_id = 101;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        ctx.last_sender_id = 1001;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);

        ctx.last_sender_id = 140;
        let mut text_input_handler = crate::handler::text_input::TextInputV1Handler;
        text_input_handler.on_keysym(&mut ctx, 501, 2, xkb::keysyms::KEY_space, WL_KEY_PRESSED, 0);
        text_input_handler.on_keysym(
            &mut ctx,
            502,
            3,
            xkb::keysyms::KEY_space,
            WL_KEY_RELEASED,
            0,
        );
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert!(
            ctx.host_to_client_queue
                .iter()
                .all(|message| message_sender(message) == 11),
            "keysym delivery must use the keyboard owning the latest matching peek"
        );

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 2000;
        crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1);
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(message_sender(&ctx.host_to_client_queue[0]), 11);
        assert_eq!(
            ctx.guest_key_owner(HostId(101), space),
            Some(GuestKeyOwner::ImeRecovery)
        );
        assert!(ctx.guest_key_owner(HostId(100), space).is_none());

        let recovered_messages = ctx.host_to_client_queue.len();
        ctx.last_sender_id = 140;
        text_input_handler.on_keysym(&mut ctx, 503, 4, xkb::keysyms::KEY_space, WL_KEY_PRESSED, 0);
        text_input_handler.on_keysym(
            &mut ctx,
            504,
            5,
            xkb::keysyms::KEY_space,
            WL_KEY_RELEASED,
            0,
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            recovered_messages,
            "delayed keysym events must see suppression on the same keyboard owner"
        );
    }

    #[test]
    fn newer_key_on_another_keyboard_remains_the_causal_watermark_after_release() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        map_keyboard(&mut ctx, 11, 101, 1001, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        focus_keyboard(&mut ctx, 100, 1, 900);
        focus_keyboard(&mut ctx, 101, 1, 900);

        ctx.last_sender_id = 100;
        let first_keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&first_keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        ctx.last_sender_id = 101;
        let second_keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let shift =
            find_keycode(&second_keymap, xkb::keysyms::KEY_Shift_L).expect("Shift in keymap");
        assert!(!ctx.keyboard_repeatable_keys[&HostId(101)].contains(&shift));

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
        ctx.last_sender_id = 1001;
        keyboard_handler.on_peek_key(&mut ctx, 2, 501, shift, WL_KEY_PRESSED);

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());

        ctx.last_sender_id = 1001;
        keyboard_handler.on_peek_key(&mut ctx, 3, 502, shift, WL_KEY_RELEASED);
        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "releasing a newer key must not revive an older held generation"
        );
    }

    #[test]
    fn keyboard_retirement_removes_its_watermark_and_recovers_other_held_key() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        map_keyboard(&mut ctx, 11, 101, 1001, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        focus_keyboard(&mut ctx, 100, 1, 900);
        focus_keyboard(&mut ctx, 101, 1, 900);

        ctx.last_sender_id = 100;
        let first_keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&first_keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        ctx.last_sender_id = 101;
        let second_keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let shift =
            find_keycode(&second_keymap, xkb::keysyms::KEY_Shift_L).expect("Shift in keymap");

        // Keyboard 100 owns an older repeatable held key. Keyboard 101 then
        // becomes the causal watermark with a non-repeatable key.
        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 500, space, WL_KEY_PRESSED);
        ctx.last_sender_id = 1001;
        keyboard_handler.on_peek_key(&mut ctx, 2, 501, shift, WL_KEY_PRESSED);
        ctx.last_sender_id = 2000;
        crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1);
        assert!(ctx.host_to_client_queue.is_empty());

        // Destroying keyboard 101 makes its generation permanently unable to
        // produce another event. Its watermark must disappear with it so the
        // still-live keyboard 100 can continue the held-Space repeat.
        ctx.last_sender_id = 11;
        assert_eq!(keyboard_handler.on_release(&mut ctx), Action::Forward);
        ctx.client_to_host_queue.clear();

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            3,
            "the remaining held key must emit a balanced pair and text-input done"
        );
        assert_eq!(message_sender(&ctx.host_to_client_queue[0]), 10);
        assert_eq!(message_sender(&ctx.host_to_client_queue[1]), 10);
        assert_eq!(
            ctx.guest_key_owner(HostId(100), space),
            Some(GuestKeyOwner::ImeRecovery)
        );
    }

    #[test]
    fn repeated_peek_key_refreshes_the_synthetic_event_time() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 100, space, WL_KEY_PRESSED);
        keyboard_handler.on_peek_key(&mut ctx, 2, 200, space, WL_KEY_REPEATED);

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        for message in &ctx.host_to_client_queue[..2] {
            let (_, time, recovered_key, _) = keyboard_event_payload(message);
            assert_eq!(time, 200);
            assert_eq!(recovered_key, space);
        }
    }

    #[test]
    fn duplicate_pressed_peek_refreshes_the_synthetic_event_time() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 1, 100, space, WL_KEY_PRESSED);
        keyboard_handler.on_peek_key(&mut ctx, 2, 200, space, WL_KEY_PRESSED);
        assert_eq!(
            ctx.key_generations
                .peek(HostId(100), space)
                .expect("peek generation")
                .sequence,
            1,
            "a duplicate pressed event must not create a new physical generation"
        );

        ctx.last_sender_id = 2000;
        crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1);
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        for message in &ctx.host_to_client_queue[..2] {
            let (_, time, recovered_key, _) = keyboard_event_payload(message);
            assert_eq!(time, 200);
            assert_eq!(recovered_key, space);
        }
    }

    #[test]
    fn accelerator_peek_generation_stays_ineligible_after_modifier_release() {
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>space").unwrap(),
        );
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        focus_keyboard(&mut ctx, 100, 1, 900);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        keyboard_handler.on_modifiers(&mut ctx, 1, ctrl_mask, 0, 0, 0);

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 2, 500, space, WL_KEY_PRESSED);
        assert!(
            !ctx.key_generations
                .peek(HostId(100), space)
                .expect("peek generation")
                .eligible
        );

        // Releasing Ctrl must not turn the same physical Space generation
        // from a host accelerator into a recoverable text key.
        ctx.last_sender_id = 100;
        keyboard_handler.on_modifiers(&mut ctx, 3, 0, 0, 0, 0);
        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn delayed_accelerator_peek_stays_ineligible_after_modifier_release() {
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>space").unwrap(),
        );
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        focus_keyboard(&mut ctx, 100, 1, 900);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        keyboard_handler.on_modifiers(&mut ctx, 1, ctrl_mask, 0, 0, 0);

        // The normal accelerator path may race ahead of peek_key. Preserve
        // that source decision even if the modifier event changes before the
        // delayed peek for the same physical generation arrives.
        assert_eq!(
            keyboard_handler.on_key(&mut ctx, 2, 500, space, WL_KEY_PRESSED),
            Action::Drop
        );
        keyboard_handler.on_modifiers(&mut ctx, 3, 0, 0, 0, 0);
        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 4, 501, space, WL_KEY_PRESSED);
        assert!(
            !ctx.key_generations
                .peek(HostId(100), space)
                .expect("peek generation")
                .eligible
        );

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn modified_non_accelerator_repeat_remains_recoverable() {
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>space").unwrap(),
        );
        let mut keyboard_handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.last_sender_id = 100;
        let keymap = load_test_keymap(&mut keyboard_handler, &mut ctx);
        let space = find_keycode(&keymap, xkb::keysyms::KEY_space).expect("Space in keymap");
        let shift_mask = 1 << keymap.mod_get_index("Shift");
        keyboard_handler.on_modifiers(&mut ctx, 1, shift_mask, 0, 0, 0);

        ctx.last_sender_id = 1000;
        keyboard_handler.on_peek_key(&mut ctx, 2, 500, space, WL_KEY_PRESSED);
        assert!(
            ctx.key_generations
                .peek(HostId(100), space)
                .expect("peek generation")
                .eligible
        );

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::handler::text_input::ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        let (_, _, recovered_key, _) = keyboard_event_payload(&ctx.host_to_client_queue[0]);
        assert_eq!(recovered_key, space);
    }

    #[test]
    fn forwarded_backspace_release_is_not_dropped_by_ime_repeat_fallback() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);

        // The initial physical press reached the guest.
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 500, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Forward
        );

        // Exo later consumes repeat confirmations. The real release still
        // has to reach the guest to close the press/release pair.
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(100), EVDEV_KEY_BACKSPACE, 40));
        assert_eq!(
            handler.on_key(&mut ctx, 2, 501, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED),
            Action::Forward
        );
    }

    #[test]
    fn non_backspace_press_cancels_repeat_without_losing_physical_backspace() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.key_generations.observe_physical_state(
            HostId(100),
            EVDEV_KEY_BACKSPACE,
            WL_KEY_PRESSED,
        );
        assert!(ctx.claim_guest_key(HostId(100), EVDEV_KEY_BACKSPACE, GuestKeyOwner::Physical));
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(100), EVDEV_KEY_BACKSPACE, 40));

        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 500, 30, WL_KEY_PRESSED),
            Action::Forward
        );
        assert!(
            ctx.key_generations
                .physically_held(HostId(100), EVDEV_KEY_BACKSPACE),
            "pressing another key must not falsify the physical Backspace state"
        );
        assert!(
            !crate::handler::text_input::backspace_repeat_active_for_keyboard(&ctx, HostId(100))
        );

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler::on_confirm_preedit(
                &mut crate::handler::text_input::ExtendedTextInputV1Handler,
                &mut ctx,
                0,
            ),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a cancelled repeat must not rearm while Backspace remains physically held"
        );

        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 2, 501, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED,),
            Action::Forward
        );
        assert!(
            !ctx.key_generations
                .physically_held(HostId(100), EVDEV_KEY_BACKSPACE),
            "Backspace release must remove only the released physical key"
        );
        assert!(
            !ctx.key_generations
                .backspace_repeat_cancelled(HostId(100), EVDEV_KEY_BACKSPACE),
            "Backspace release must clear repeat cancellation"
        );
    }

    #[test]
    fn backspace_auto_repeat_press_does_not_rearm_cancelled_repeat() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        ctx.key_generations.observe_physical_state(
            HostId(100),
            EVDEV_KEY_BACKSPACE,
            WL_KEY_PRESSED,
        );
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(100), EVDEV_KEY_BACKSPACE, 40));

        // A newer key cancels the IME repeat while the physical Backspace
        // remains held.
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 500, 30, WL_KEY_PRESSED),
            Action::Forward
        );
        assert!(ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(100), EVDEV_KEY_BACKSPACE));

        // Wayland represents key auto-repeat as additional pressed events.
        // They must not clear the cancellation until the physical release.
        assert_eq!(
            handler.on_key(&mut ctx, 2, 501, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Forward
        );
        assert!(
            ctx.key_generations
                .backspace_repeat_cancelled(HostId(100), EVDEV_KEY_BACKSPACE),
            "a repeated Backspace press must not rearm a cancelled IME repeat"
        );

        ctx.last_sender_id = 2000;
        assert_eq!(
            crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler::on_confirm_preedit(
                &mut crate::handler::text_input::ExtendedTextInputV1Handler,
                &mut ctx,
                0,
            ),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a repeated Backspace press must not synthesize another key pair"
        );
    }

    #[test]
    fn keyboard_enter_initializes_held_key_state() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        ctx.shadow_table.map_id(20, 200);
        ctx.last_sender_id = 100;

        let keys = EVDEV_KEY_BACKSPACE.to_ne_bytes();
        assert_eq!(handler.on_enter(&mut ctx, 1, 200, &keys), Action::Forward);
        assert!(
            ctx.key_generations
                .physically_held(HostId(100), EVDEV_KEY_BACKSPACE),
            "wl_keyboard.enter keys must seed the physical held-key state"
        );
        assert!(
            ctx.guest_key_owner(HostId(100), EVDEV_KEY_BACKSPACE) == Some(GuestKeyOwner::Physical),
            "keys announced to the guest in enter still require real releases"
        );
    }

    #[test]
    fn duplicate_keyboard_enter_does_not_reset_ime_focus() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        ctx.shadow_table.map_id(20, 200);
        focus_keyboard(&mut ctx, 100, 1, 20);
        add_active_text_input(&mut ctx, 40, 1, 2000);
        {
            let state = ctx.text_inputs.get_mut(&40).expect("text input");
            state.active_surface = Some(20);
            state.current_preedit = "한".to_string();
            state.committed_enabled = true;
            state.host_activation = crate::state::HostActivationState::Active;
        }
        ctx.last_sender_id = 100;

        assert_eq!(handler.on_enter(&mut ctx, 2, 200, &[]), Action::Drop);
        assert_eq!(ctx.text_inputs[&40].active_surface, Some(20));
        assert_eq!(ctx.text_inputs[&40].current_preedit, "한");
        assert!(ctx.text_inputs[&40].host_is_active());
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "duplicate enter must not emit a second text-input enter"
        );
    }

    #[test]
    fn duplicate_keyboard_enter_preserves_pressed_key_state() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        ctx.shadow_table.map_id(20, 200);
        ctx.last_sender_id = 100;

        assert_eq!(handler.on_enter(&mut ctx, 1, 200, &[]), Action::Forward);
        assert_eq!(
            handler.on_key(&mut ctx, 2, 10, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Forward
        );
        assert!(ctx
            .key_generations
            .physically_held(HostId(100), EVDEV_KEY_BACKSPACE));

        // A compositor may repeat enter for the same resource while the key is
        // held. The empty keys array must not erase the physical state needed
        // to pair the eventual release.
        assert_eq!(handler.on_enter(&mut ctx, 3, 200, &[]), Action::Drop);
        assert!(
            ctx.key_generations
                .physically_held(HostId(100), EVDEV_KEY_BACKSPACE),
            "duplicate enter must not clear held keys"
        );
        assert_eq!(
            handler.on_key(&mut ctx, 4, 11, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED),
            Action::Forward
        );
    }

    #[test]
    fn direct_focus_replacement_sends_balanced_leave_then_enter() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        map_keyboard(&mut ctx, 11, 101, 1001, 1);
        ctx.shadow_table.map_id(20, 200);
        ctx.shadow_table.map_id(21, 201);
        add_active_text_input(&mut ctx, 40, 1, 2000);

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_enter(&mut ctx, 1, 200, &[]), Action::Forward);
        ctx.host_to_client_queue.clear();

        ctx.last_sender_id = 101;
        assert_eq!(handler.on_enter(&mut ctx, 2, 201, &[]), Action::Forward);
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(message_sender(&ctx.host_to_client_queue[0]), 40);
        assert_eq!(message_opcode(&ctx.host_to_client_queue[0]), 1);
        assert_eq!(message_first_u32(&ctx.host_to_client_queue[0]), 20);
        assert_eq!(message_sender(&ctx.host_to_client_queue[1]), 40);
        assert_eq!(message_opcode(&ctx.host_to_client_queue[1]), 0);
        assert_eq!(message_first_u32(&ctx.host_to_client_queue[1]), 21);
        assert_eq!(ctx.text_inputs[&40].active_surface, Some(21));
        assert_eq!(ctx.keyboard_focus.surface_for_seat(1), Some(21));
        assert!(
            ctx.keyboard_focus.focus_for_keyboard(HostId(100)).is_none(),
            "a newer surface must retire older keyboard generations on the seat"
        );

        let message_count = ctx.host_to_client_queue.len();
        ctx.last_sender_id = 100;
        assert_eq!(handler.on_leave(&mut ctx, 3, 200), Action::Forward);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            message_count,
            "the delayed old leave must be idempotent"
        );
        assert_eq!(ctx.keyboard_focus.surface_for_seat(1), Some(21));

        assert_eq!(
            handler.on_leave(&mut ctx, 4, 200),
            Action::Drop,
            "the retired guest enter can be balanced only once"
        );
        assert_eq!(ctx.host_to_client_queue.len(), message_count);
    }

    #[test]
    fn leave_from_one_keyboard_keeps_same_surface_focused_for_another() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        map_keyboard(&mut ctx, 11, 101, 1001, 1);
        ctx.shadow_table.map_id(20, 200);
        add_active_text_input(&mut ctx, 40, 1, 2000);

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_enter(&mut ctx, 1, 200, &[]), Action::Forward);
        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 101;
        assert_eq!(
            handler.on_enter(&mut ctx, 2, 200, &[]),
            Action::Forward,
            "each wl_keyboard resource needs its own enter"
        );
        ctx.host_to_client_queue.clear();
        assert_eq!(ctx.keyboard_focus.surface_for_seat(1), Some(20));

        // The first keyboard leaves, but the second one still owns the same
        // surface. Seat-level IME focus must remain active.
        ctx.last_sender_id = 100;
        assert_eq!(handler.on_leave(&mut ctx, 3, 200), Action::Forward);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(1), Some(20));
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "the remaining keyboard must keep text-input focus"
        );

        ctx.last_sender_id = 101;
        assert_eq!(handler.on_leave(&mut ctx, 4, 200), Action::Forward);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(1), None);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
    }

    #[test]
    fn same_keyboard_reenter_drops_delayed_old_leave() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = KeyboardHandler::new();
        map_keyboard(&mut ctx, 10, 100, 1000, 1);
        ctx.shadow_table.map_id(20, 200);
        ctx.shadow_table.map_id(21, 201);
        add_active_text_input(&mut ctx, 40, 1, 2000);

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_enter(&mut ctx, 1, 200, &[]), Action::Forward);
        assert_eq!(handler.on_enter(&mut ctx, 2, 201, &[]), Action::Forward);
        let message_count = ctx.host_to_client_queue.len();

        assert_eq!(
            handler.on_leave(&mut ctx, 3, 200),
            Action::Drop,
            "an old leave must not clear a newer enter on the same resource"
        );
        assert_eq!(ctx.host_to_client_queue.len(), message_count);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(1), Some(21));
        assert_eq!(ctx.text_inputs[&40].active_surface, Some(21));
    }

    #[test]
    fn invalid_keymap_clears_context_keyboard_state() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        ctx.last_sender_id = 5;
        load_test_keymap(&mut handler, &mut ctx);
        ctx.key_generations
            .observe_peek_press(HostId(5), EVDEV_KEY_BACKSPACE, 1, 123, true);
        ctx.key_generations
            .cancel_backspace_repeat(HostId(5), EVDEV_KEY_BACKSPACE);
        assert!(ctx.claim_guest_key(HostId(5), EVDEV_KEY_BACKSPACE, GuestKeyOwner::ImeRecovery));

        assert_eq!(handler.on_keymap(&mut ctx, 99, 0, 0), Action::Forward);
        assert!(!ctx
            .key_generations
            .physically_held(HostId(5), EVDEV_KEY_BACKSPACE));
        assert!(!ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(5), EVDEV_KEY_BACKSPACE));
        assert!(ctx
            .key_generations
            .peek(HostId(5), EVDEV_KEY_BACKSPACE)
            .is_none());
        assert!(!ctx.keyboard_repeatable_keys.contains_key(&HostId(5)));
        assert!(!handler.modifiers.contains_key(&HostId(5)));
        assert!(ctx
            .guest_key_owner(HostId(5), EVDEV_KEY_BACKSPACE)
            .is_none());
    }

    #[test]
    fn accelerator_drop_state_does_not_cross_keyboards() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );
        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);

        ctx.last_sender_id = 5;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 10, wl_key_a, WL_KEY_PRESSED),
            Action::Drop
        );

        ctx.last_sender_id = 6;
        assert_eq!(
            handler.on_key(&mut ctx, 2, 11, wl_key_a, WL_KEY_RELEASED),
            Action::Drop,
            "keyboard B must not inherit keyboard A's dropped press; an untracked release is dropped"
        );

        ctx.last_sender_id = 5;
        assert_eq!(
            handler.on_key(&mut ctx, 3, 12, wl_key_a, WL_KEY_RELEASED),
            Action::Drop,
            "keyboard A's matching release must remain dropped"
        );
    }

    #[test]
    fn accelerator_modifier_state_is_scoped_to_keyboard() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );

        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);

        ctx.last_sender_id = 6;
        load_test_keymap(&mut handler, &mut ctx);
        assert_eq!(
            handler.on_key(&mut ctx, 1, 10, wl_key_a, WL_KEY_PRESSED),
            Action::Forward,
            "keyboard B must not inherit keyboard A's Control modifier"
        );

        ctx.last_sender_id = 5;
        assert_eq!(
            handler.on_key(&mut ctx, 2, 11, wl_key_a, WL_KEY_PRESSED),
            Action::Drop,
            "keyboard A must retain its own Control modifier"
        );
    }

    #[test]
    fn opcode_encoding_get_extended_keyboard_is_zero() {
        // Regression: get_extended_keyboard uses opcode 0. Verify the wire
        // message encodes it correctly (not accidentally a non-zero opcode).
        let mut ctx = Context::new(false, false);
        ctx.host_keyboard_extension_id = Some(HostId(99));

        KeyboardHandler::ensure_extended_keyboard_bound(&mut ctx, HostId(10));
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let (msg, _) = &ctx.client_to_host_queue[0];
        let word2 = u32::from_ne_bytes(msg[4..8].try_into().unwrap());
        let opcode = word2 & 0xFFFF;
        assert_eq!(
            opcode, 0,
            "get_extended_keyboard must use opcode 0, got {}",
            opcode
        );
    }

    /// Accelerator suppression must be cleared when the keyboard is released.
    ///
    /// Without clearing suppression in `on_release`, a re-created keyboard (guest
    /// destroys and re-creates wl_keyboard, which is common on focus changes) inherits
    /// stale drop state and silently swallows release events for keys it never saw pressed,
    /// leaving the guest in a stuck-key state.
    #[test]
    fn accelerator_suppression_cleared_on_release() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );

        ctx.last_sender_id = 10;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");

        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(10), HostId(50));

        // Press Ctrl+A (host accelerator) with host keyboard ID 10.
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);
        ctx.last_sender_id = 10; // host keyboard ID (on_key is host→client)
        let action = handler.on_key(&mut ctx, 1, 0, wl_key_a, WL_KEY_PRESSED);
        assert_eq!(action, Action::Drop);
        assert!(
            ctx.key_generations
                .host_accelerator_suppressed(HostId(10), wl_key_a),
            "key must be suppressed after accelerator handling"
        );

        // Guest destroys the keyboard (client→host: last_sender_id is the guest ID).
        ctx.shadow_table.map_id(5, 10); // guest 5 ↔ host 10
        ctx.last_sender_id = 5;
        handler.on_release(&mut ctx);

        // Accelerator suppression must be empty after release.
        assert!(
            !ctx.key_generations
                .host_accelerator_suppressed(HostId(10), wl_key_a),
            "accelerator suppression must be cleared by on_release"
        );

        // A new press on the re-bound keyboard should now be evaluated with
        // fresh state rather than inheriting the stale accelerator drop.
        ctx.last_sender_id = 10;
        let press_action = handler.on_key(&mut ctx, 2, 0, wl_key_a, WL_KEY_PRESSED);
        assert_eq!(
            press_action,
            Action::Forward,
            "new press must be forwarded after re-bind"
        );
        assert_eq!(
            handler.on_key(&mut ctx, 3, 0, wl_key_a, WL_KEY_RELEASED),
            Action::Forward,
            "release paired with the new forwarded press must be forwarded"
        );
    }

    /// Structural test: `MessageBuilder::build_message` must produce well-formed wire frames.
    ///
    /// This catches any future refactoring that breaks the `[sender][size<<16|opcode][payload]`
    /// encoding, which would silently corrupt all internal protocol messages.
    #[test]
    fn build_wayland_msg_encodes_frame_correctly() {
        let payload = [0x01u8, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00]; // two u32 words
        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_u32(0x00000001);
        builder.write_u32(0x00000002);
        let msg = builder.build_message(42, 7);

        let sender = u32::from_ne_bytes(msg[0..4].try_into().unwrap());
        assert_eq!(sender, 42, "sender_id wrong");

        let expected_total_len = 8u32 + payload.len() as u32; // header + payload
        let word2 = u32::from_ne_bytes(msg[4..8].try_into().unwrap());
        assert_eq!(word2 >> 16, expected_total_len, "size field wrong");
        assert_eq!(word2 & 0xFFFF, 7, "opcode field wrong");

        assert_eq!(&msg[8..], &payload, "payload bytes wrong");
    }

    /// Validate that our hand-written opcode constants match the values the
    /// code-generator derives from the XML. If the XML is ever updated and
    /// the generated opcodes change, this test fails \u2014 preventing the silent
    /// serialization corruption that would otherwise occur.
    ///
    /// The generated `Request::opcode()` method is the authoritative source:
    ///   zcr_extended_keyboard_v1::Request::Destroy => 0
    ///   zcr_extended_keyboard_v1::Request::AckKey  => 1
    ///   zcr_keyboard_extension_v1::Request::GetExtendedKeyboard => 0
    #[test]
    fn opcode_constants_match_generated_protocol() {
        use crate::protocols::keyboard_extension_unstable_v1::{
            zcr_extended_keyboard_v1::Request as ExtKbReq,
            zcr_keyboard_extension_v1::Request as ExtFactReq,
        };

        assert_eq!(
            ExtKbReq::Destroy {}.opcode(),
            ZCR_EXTENDED_KEYBOARD_DESTROY,
            "ZCR_EXTENDED_KEYBOARD_DESTROY constant out of sync with generated protocol"
        );
        assert_eq!(
            ExtKbReq::AckKey {
                serial: 0,
                handled: 0
            }
            .opcode(),
            ZCR_EXTENDED_KEYBOARD_ACK_KEY,
            "ZCR_EXTENDED_KEYBOARD_ACK_KEY constant out of sync with generated protocol"
        );
        assert_eq!(
            ExtFactReq::GetExtendedKeyboard { id: 0, keyboard: 0 }.opcode(),
            ZCR_KEYBOARD_EXTENSION_GET_EXTENDED_KEYBOARD,
            "ZCR_KEYBOARD_EXTENSION_GET_EXTENDED_KEYBOARD constant out of sync with generated protocol"
        );
    }

    /// Accelerator suppression must be cleared when a new keymap arrives so
    /// that keysym re-mappings across keymap updates cannot leave stale drop
    /// entries that would cause stuck keys.
    #[test]
    fn accelerator_suppression_cleared_on_keymap_reload() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(
            false,
            false,
            crate::accelerator::parse_accelerators("<Control>a").unwrap(),
        );

        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");

        // Simulate dropping a key (press of Ctrl+A)
        let ctrl_mask = 1 << keymap.mod_get_index("Control");
        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(5), HostId(50));
        ctx.last_sender_id = 5;
        let action = handler.on_key(&mut ctx, 1, 0, wl_key_a, WL_KEY_PRESSED);
        assert_eq!(action, Action::Drop);
        assert!(ctx
            .key_generations
            .host_accelerator_suppressed(HostId(5), wl_key_a));

        // Reload keymap: accelerator suppression must be cleared.
        load_test_keymap(&mut handler, &mut ctx);
        assert!(
            !ctx.key_generations
                .host_accelerator_suppressed(HostId(5), wl_key_a),
            "accelerator suppression must be cleared on keymap reload"
        );

        // A keymap replaces the XKB state, so stale modifiers from the previous
        // keymap must not be reused before the compositor sends new modifiers.
        let action = handler.on_key(&mut ctx, 2, 0, wl_key_a, WL_KEY_PRESSED);
        assert_eq!(
            action,
            Action::Forward,
            "keymap reload must clear stale modifier state"
        );

        handler.on_modifiers(&mut ctx, 0, ctrl_mask, 0, 0, 0);
        let action = handler.on_key(&mut ctx, 3, 0, wl_key_a, WL_KEY_PRESSED);
        assert_eq!(
            action,
            Action::Drop,
            "accelerator must work after fresh modifiers arrive"
        );
    }

    #[test]
    fn keymap_reload_preserves_forwarded_key_release_pairing() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        ctx.last_sender_id = 5;
        let keymap = load_test_keymap(&mut handler, &mut ctx);
        let wl_key_a =
            find_keycode(&keymap, xkb::keysyms::KEY_a).expect("KEY_a not found in keymap");

        ctx.last_sender_id = 5;
        assert_eq!(
            handler.on_key(&mut ctx, 1, 0, wl_key_a, WL_KEY_PRESSED),
            Action::Forward
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(5), wl_key_a),
            Some(GuestKeyOwner::Physical)
        );

        // A compositor may resend the keymap while the key remains held.
        ctx.last_sender_id = 5;
        load_test_keymap(&mut handler, &mut ctx);
        assert!(
            ctx.guest_key_owner(HostId(5), wl_key_a) == Some(GuestKeyOwner::Physical),
            "keymap replacement must retain the outstanding forwarded press"
        );

        assert_eq!(
            handler.on_key(&mut ctx, 2, 0, wl_key_a, WL_KEY_RELEASED),
            Action::Forward,
            "the release matching a pre-reload press must still be forwarded"
        );
        assert!(ctx.guest_key_owner(HostId(5), wl_key_a).is_none());
    }

    /// Regression: on_release must reset modifiers to 0 so a re-bound keyboard
    /// does not inherit stale modifier state from the previous session. Without
    /// this, a key event arriving before the next wl_keyboard.modifiers could
    /// be matched against the old (wrong) modifier mask.
    #[test]
    fn modifiers_reset_on_release() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);

        ctx.shadow_table.map_id(5, 10); // guest 5 <-> host 10

        // Inject a modifier state directly (no XKB state needed for this test).
        handler
            .modifiers
            .insert(HostId(10), crate::accelerator::CONTROL_MASK);
        assert_ne!(
            handler.modifiers[&HostId(10)],
            0,
            "precondition: modifiers are non-zero"
        );

        // Release the keyboard (client->host request: last_sender_id = guest ID).
        ctx.last_sender_id = 5;
        handler.on_release(&mut ctx);

        assert!(
            !handler.modifiers.contains_key(&HostId(10)),
            "modifiers must be cleared by on_release to avoid stale state on re-bind"
        );
    }

    /// N8: on_release with an unknown guest keyboard ID (keyboard that never
    /// received on_enter) must return Forward without panicking or queuing anything.
    #[test]
    fn on_release_with_unknown_guest_id_forwards_gracefully() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        // last_sender_id 999 is not in the shadow table.
        ctx.last_sender_id = 999;
        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "no destroy message should be queued for an unknown keyboard"
        );
    }

    #[test]
    fn on_release_before_host_mapping_cleans_guest_seat_routing() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new(false, false);
        ctx.keyboard_to_seat.insert(5, 7);
        ctx.last_sender_id = 5;

        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert!(
            !ctx.keyboard_to_seat.contains_key(&5),
            "release before the first host event must not leave a stale keyboard route"
        );
    }

    #[test]
    fn keyboard_release_ends_focus_when_no_other_keyboard_owns_the_seat() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_seat = 7;
        let host_seat = 70;
        let guest_keyboard = 5;
        let host_keyboard = 50;
        let guest_surface = 21;
        let host_surface = 210;
        let guest_text_input = 40;
        let host_text_input = 400;

        ctx.shadow_table.map_id(guest_seat, host_seat);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.shadow_table.map_id(guest_surface, host_surface);
        ctx.shadow_table
            .track_interface(guest_surface, "wl_surface".to_string());
        ctx.shadow_table.map_id(guest_text_input, host_text_input);
        ctx.shadow_table.track_interface_with_version(
            guest_text_input,
            "zwp_text_input_v3".to_string(),
            1,
        );
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        focus_keyboard(&mut ctx, host_keyboard, guest_seat, guest_surface);
        ctx.text_inputs.insert(
            guest_text_input,
            crate::state::TextInputState {
                host_v1_id: 401,
                host_ext_id: None,
                guest_seat,
                active_surface: Some(guest_surface),
                pending_enabled: false,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: "한".to_string(),
                guest_commit_serial: 1,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: crate::state::HostActivationState::Active,
            },
        );

        ctx.last_sender_id = guest_keyboard;
        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert!(
            ctx.keyboard_focus.surface_for_seat(guest_seat).is_none(),
            "keyboard release must not leave a stale seat focus"
        );
        assert_eq!(
            ctx.text_inputs[&guest_text_input].active_surface, None,
            "keyboard release must send text-input leave when it owns seat focus"
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "keyboard release must queue exactly one v3 leave"
        );
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "keyboard release must deactivate then fence the host v1 input"
        );
        assert_eq!(message_sender(&ctx.client_to_host_queue[0]), 401);
        assert_eq!(message_opcode(&ctx.client_to_host_queue[0]), 1);
        assert_eq!(message_sender(&ctx.client_to_host_queue[1]), 1);
        assert_eq!(
            message_opcode(&ctx.client_to_host_queue[1]),
            crate::protocols::wayland::wl_display::REQ_SYNC
        );
    }

    #[test]
    fn keyboard_release_keeps_focus_owned_by_another_keyboard() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_seat = 7;
        let guest_surface = 21;
        let guest_keyboard = 5;
        let host_keyboard = 50;
        let other_guest_keyboard = 6;
        let other_host_keyboard = 60;
        let guest_text_input = 40;

        ctx.shadow_table.map_id(guest_seat, 70);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_surface, 210);
        ctx.shadow_table
            .track_interface(guest_surface, "wl_surface".to_string());
        for (guest_keyboard_id, host_keyboard_id) in [
            (guest_keyboard, host_keyboard),
            (other_guest_keyboard, other_host_keyboard),
        ] {
            ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
            ctx.shadow_table
                .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard_id, guest_seat);
            focus_keyboard(&mut ctx, host_keyboard_id, guest_seat, guest_surface);
        }
        ctx.shadow_table.map_id(guest_text_input, 400);
        ctx.shadow_table.track_interface_with_version(
            guest_text_input,
            "zwp_text_input_v3".to_string(),
            1,
        );
        ctx.text_inputs.insert(
            guest_text_input,
            crate::state::TextInputState {
                host_v1_id: 401,
                host_ext_id: None,
                guest_seat,
                active_surface: Some(guest_surface),
                pending_enabled: false,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: "한".to_string(),
                guest_commit_serial: 1,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: crate::state::HostActivationState::Active,
            },
        );

        ctx.last_sender_id = guest_keyboard;
        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert_eq!(
            ctx.keyboard_focus.surface_for_seat(guest_seat),
            Some(guest_surface)
        );
        assert_eq!(
            ctx.text_inputs[&guest_text_input].active_surface,
            Some(guest_surface)
        );
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx
            .keyboard_focus
            .focus_for_keyboard(HostId(other_host_keyboard))
            .is_some());
    }

    #[test]
    fn keyboard_release_does_not_cancel_another_keyboard_ime_repeat() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_seat = 7;
        let guest_surface = 21;
        let guest_keyboard = 5;
        let host_keyboard = 50;
        let other_guest_keyboard = 6;
        let other_host_keyboard = 60;
        let guest_text_input = 40;

        ctx.shadow_table.map_id(guest_seat, 70);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_surface, 210);
        ctx.shadow_table
            .track_interface(guest_surface, "wl_surface".to_string());
        for (guest_keyboard_id, host_keyboard_id) in [
            (guest_keyboard, host_keyboard),
            (other_guest_keyboard, other_host_keyboard),
        ] {
            ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
            ctx.shadow_table
                .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard_id, guest_seat);
            focus_keyboard(&mut ctx, host_keyboard_id, guest_seat, guest_surface);
        }
        ctx.shadow_table.map_id(guest_text_input, 400);
        ctx.shadow_table.track_interface_with_version(
            guest_text_input,
            "zwp_text_input_v3".to_string(),
            1,
        );
        ctx.key_generations.observe_physical_state(
            HostId(other_host_keyboard),
            EVDEV_KEY_BACKSPACE,
            WL_KEY_PRESSED,
        );
        ctx.key_generations
            .cancel_backspace_repeat(HostId(other_host_keyboard), EVDEV_KEY_BACKSPACE);
        ctx.text_inputs.insert(
            guest_text_input,
            crate::state::TextInputState {
                host_v1_id: 401,
                host_ext_id: None,
                guest_seat,
                active_surface: Some(guest_surface),
                pending_enabled: false,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: "한".to_string(),
                guest_commit_serial: 1,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: crate::state::HostActivationState::Active,
            },
        );

        ctx.last_sender_id = guest_keyboard;
        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert!(ctx
            .key_generations
            .physically_held(HostId(other_host_keyboard), EVDEV_KEY_BACKSPACE));
        assert!(ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(other_host_keyboard), EVDEV_KEY_BACKSPACE));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn keyboard_release_after_seat_release_does_not_deactivate_destroyed_host_seat() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_seat = 7;
        let host_seat = 70;
        let guest_keyboard = 5;
        let host_keyboard = 50;
        let guest_surface = 21;

        ctx.shadow_table.map_id(guest_seat, host_seat);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.shadow_table.map_id(guest_surface, 210);
        ctx.shadow_table
            .track_interface(guest_surface, "wl_surface".to_string());
        ctx.shadow_table.map_id(40, 400);
        ctx.shadow_table
            .track_interface_with_version(40, "zwp_text_input_v3".to_string(), 1);
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        focus_keyboard(&mut ctx, host_keyboard, guest_seat, guest_surface);
        ctx.text_inputs.insert(
            40,
            crate::state::TextInputState {
                host_v1_id: 401,
                host_ext_id: None,
                guest_seat,
                active_surface: Some(guest_surface),
                pending_enabled: false,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: String::new(),
                guest_commit_serial: 1,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: crate::state::HostActivationState::Active,
            },
        );

        // Parent seat release has already invalidated the host seat proxy.
        ctx.shadow_table.mark_pending_destroy(guest_seat);
        ctx.last_sender_id = guest_keyboard;
        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "do not send v1 deactivate through a released host seat"
        );
        assert!(!ctx.text_inputs[&40].host_is_active());
    }

    #[test]
    fn keyboard_release_invalidates_all_same_seat_text_inputs_and_delayed_leave_is_not_duplicate() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_seat = 7;
        let guest_keyboard = 5;
        let host_keyboard = 50;
        let released_surface = 21;
        let delayed_host_surface = 210;
        let other_surface = 22;

        ctx.shadow_table.map_id(guest_seat, 70);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        for (guest_surface, host_surface) in [
            (released_surface, delayed_host_surface),
            (other_surface, 220),
        ] {
            ctx.shadow_table.map_id(guest_surface, host_surface);
            ctx.shadow_table
                .track_interface(guest_surface, "wl_surface".to_string());
        }
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        focus_keyboard(&mut ctx, host_keyboard, guest_seat, released_surface);

        for (guest_text_input, active_surface) in [
            (40, Some(released_surface)),
            (41, Some(other_surface)),
            (42, None),
        ] {
            ctx.shadow_table
                .map_id(guest_text_input, guest_text_input + 400);
            ctx.shadow_table.track_interface_with_version(
                guest_text_input,
                "zwp_text_input_v3".to_string(),
                1,
            );
            ctx.text_inputs.insert(
                guest_text_input,
                crate::state::TextInputState {
                    host_v1_id: guest_text_input + 500,
                    host_ext_id: None,
                    guest_seat,
                    active_surface,
                    pending_enabled: false,
                    committed_enabled: true,
                    enabled_dirty: false,
                    pending_surrounding_text: None,
                    committed_surrounding_text: None,
                    surrounding_text_dirty: false,
                    content_hint: 0,
                    content_purpose: 0,
                    committed_content_type: None,
                    content_type_dirty: false,
                    cursor_rect: None,
                    cursor_rect_dirty: false,
                    text_change_cause: 0,
                    current_preedit: String::new(),
                    guest_commit_serial: 1,
                    pending_preedit_cursor: None,
                    pending_preedit_selection: None,
                    pending_deletes: Vec::new(),
                    pending_cursor_position: None,
                    host_activation: crate::state::HostActivationState::Active,
                },
            );
        }

        ctx.last_sender_id = guest_keyboard;
        assert_eq!(handler.on_release(&mut ctx), Action::Forward);
        assert!(ctx
            .text_inputs
            .values()
            .all(|state| state.active_surface.is_none()));
        assert!(ctx.text_inputs.values().all(|state| {
            !state.pending_enabled
                && !state.committed_enabled
                && !state.host_is_active()
                && state.current_preedit.is_empty()
        }));
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "only focused text inputs should receive leave events"
        );
        let deactivated_host_inputs = ctx
            .client_to_host_queue
            .iter()
            .filter(|message| {
                message_opcode(message) == 1 && [540, 541, 542].contains(&message_sender(message))
            })
            .count();
        assert_eq!(
            deactivated_host_inputs, 3,
            "every active host v1 input must be deactivated, including a stale local None focus"
        );

        let guest_queue_len = ctx.host_to_client_queue.len();
        let host_queue_len = ctx.client_to_host_queue.len();
        ctx.last_sender_id = host_keyboard;
        assert_eq!(
            handler.on_leave(&mut ctx, 1, delayed_host_surface),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), guest_queue_len);
        assert_eq!(ctx.client_to_host_queue.len(), host_queue_len);
    }

    #[test]
    fn keyboard_enter_invalidates_stale_text_input_before_new_enable() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(5, 10);
        ctx.keyboard_to_seat.insert(5, 7);
        // Text-input v1 activation needs a real host-side wl_seat mapping.
        // The keyboard route above identifies guest seat 7, so model the
        // corresponding host seat as well.
        ctx.shadow_table.map_id(7, 2);
        ctx.shadow_table.track_interface(7, "wl_seat".to_string());
        ctx.shadow_table.map_id(20, 30);
        ctx.text_inputs.insert(
            40,
            crate::state::TextInputState {
                host_v1_id: 50,
                host_ext_id: None,
                guest_seat: 7,
                active_surface: Some(21),
                pending_enabled: true,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: Some(("stale".to_string(), 5, 5)),
                committed_surrounding_text: Some(("stale".to_string(), 5, 5)),
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: "한".to_string(),
                guest_commit_serial: 9,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: vec![(3, 0)],
                pending_cursor_position: None,
                host_activation: crate::state::HostActivationState::Active,
            },
        );
        ctx.last_sender_id = 10;

        assert_eq!(handler.on_enter(&mut ctx, 1, 30, &[]), Action::Forward);

        let state = &ctx.text_inputs[&40];
        assert_eq!(state.active_surface, Some(20));
        assert!(!state.pending_enabled);
        assert!(!state.committed_enabled);
        assert!(!state.host_is_active());
        assert!(state.committed_surrounding_text.is_none());
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_deletes.is_empty());
        assert_eq!(state.guest_commit_serial, 9);

        let deactivate = ctx.client_to_host_queue.iter().find(|(message, _)| {
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) == 50
                && (u32::from_ne_bytes(message[4..8].try_into().unwrap()) & 0xffff) == 1
        });
        assert!(deactivate.is_some());
        assert!(ctx.host_to_client_queue.iter().any(|(message, _)| {
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) == 40
                && (u32::from_ne_bytes(message[4..8].try_into().unwrap()) & 0xffff) == 0
        }));

        ctx.key_generations
            .observe_physical_state(HostId(10), EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        ctx.last_sender_id = 10;
        assert_eq!(
            handler.on_key(&mut ctx, 2, 10, 30, WL_KEY_PRESSED),
            Action::Forward
        );
        assert!(ctx
            .key_generations
            .physically_held(HostId(10), EVDEV_KEY_BACKSPACE));
        assert!(ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(10), EVDEV_KEY_BACKSPACE));
        assert!(
            !crate::handler::text_input::backspace_repeat_active_for_keyboard(&ctx, HostId(10))
        );

        ctx.last_sender_id = 10;
        // The previous physical Backspace session ended before the new
        // transaction.  Its release clears the cancellation marker; the
        // following press is the newly-held physical key that the IME
        // fallback is allowed to consume.
        assert_eq!(
            handler.on_key(&mut ctx, 5, 13, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED),
            Action::Drop
        );
        assert!(!ctx
            .key_generations
            .physically_held(HostId(10), EVDEV_KEY_BACKSPACE));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(100), HostId(10));
        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_peek_key(&mut ctx, 6, 14, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Drop
        );
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(10), EVDEV_KEY_BACKSPACE, 40));
        ctx.last_sender_id = 10;
        assert_eq!(
            handler.on_key(&mut ctx, 6, 14, EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED),
            Action::Drop
        );
        assert!(ctx
            .key_generations
            .physically_held(HostId(10), EVDEV_KEY_BACKSPACE));
        ctx.last_sender_id = 100;
        handler.on_peek_key(&mut ctx, 7, 15, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED);
        ctx.last_sender_id = 10;
        assert_eq!(
            handler.on_key(&mut ctx, 7, 15, EVDEV_KEY_BACKSPACE, WL_KEY_RELEASED),
            Action::Drop
        );
        assert!(!ctx
            .key_generations
            .physically_held(HostId(10), EVDEV_KEY_BACKSPACE));
        assert!(
            !crate::handler::text_input::backspace_repeat_active_for_keyboard(&ctx, HostId(10))
        );
    }

    #[test]
    fn stale_leave_does_not_end_current_seat_backspace_repeat() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);

        map_keyboard(&mut ctx, 5, 10, 50, 7);
        ctx.shadow_table.map_id(20, 30);
        ctx.shadow_table.map_id(21, 31);
        focus_keyboard(&mut ctx, 10, 7, 20);
        add_active_text_input(&mut ctx, 40, 7, 50);
        let state = ctx.text_inputs.get_mut(&40).expect("text input");
        state.active_surface = Some(20);
        ctx.key_generations
            .observe_physical_state(HostId(10), EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(10), EVDEV_KEY_BACKSPACE, 40));

        // The old keyboard object reports leave for surface 21 after surface
        // 20 is already focused. This event must not tear down seat-level IME
        // state or send a v3 leave for the current focus.
        ctx.last_sender_id = 10;
        assert_eq!(handler.on_leave(&mut ctx, 1, 31), Action::Drop);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(7), Some(20));
        assert!(crate::handler::text_input::backspace_repeat_active_for_keyboard(&ctx, HostId(10)));
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "stale leave must not emit a text-input leave"
        );

        // A matching leave still ends the fallback and clears focus.
        ctx.last_sender_id = 10;
        assert_eq!(handler.on_leave(&mut ctx, 2, 30), Action::Forward);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(7), None);
        assert!(
            !crate::handler::text_input::backspace_repeat_active_for_keyboard(&ctx, HostId(10))
        );
    }

    #[test]
    fn mapped_stale_leave_clears_old_keyboard_state_without_disturbing_new_focus() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);

        // Keyboard A is leaving its old surface while keyboard B already owns
        // a different current surface on the same seat. The leave is mapped
        // (unlike the destroyed-surface case below), so this ordering must
        // still retire A's physical/IME state before returning.
        map_keyboard(&mut ctx, 5, 10, 50, 7);
        map_keyboard(&mut ctx, 6, 11, 51, 7);
        ctx.shadow_table.map_id(20, 30);
        ctx.shadow_table.map_id(21, 31);
        focus_keyboard(&mut ctx, 10, 7, 20);
        ctx.key_generations
            .observe_peek_press(HostId(10), EVDEV_KEY_BACKSPACE, 1, 123, true);
        ctx.key_generations
            .cancel_backspace_repeat(HostId(10), EVDEV_KEY_BACKSPACE);
        assert!(ctx.claim_guest_key(HostId(10), EVDEV_KEY_BACKSPACE, GuestKeyOwner::ImeRecovery));
        ctx.key_generations
            .suppress_host_accelerator(HostId(10), EVDEV_KEY_BACKSPACE);
        handler.modifiers.insert(HostId(10), 0xdead_beef);

        // Entering a new surface is the transition that retires the older
        // keyboard generation. The later leave must already be a no-op.
        ctx.last_sender_id = 11;
        assert_eq!(handler.on_enter(&mut ctx, 1, 31, &[]), Action::Forward);
        let queued_after_enter = ctx.host_to_client_queue.len();
        ctx.last_sender_id = 10;
        assert_eq!(handler.on_leave(&mut ctx, 2, 30), Action::Forward);
        assert_eq!(ctx.host_to_client_queue.len(), queued_after_enter);

        assert!(
            ctx.keyboard_focus.focus_for_keyboard(HostId(10)).is_none(),
            "the stale keyboard's focus association must be retired"
        );
        assert_eq!(
            ctx.keyboard_focus
                .focus_for_keyboard(HostId(11))
                .map(|focus| focus.guest_surface),
            Some(21),
            "the other keyboard's current focus must remain intact"
        );
        assert_eq!(ctx.keyboard_focus.surface_for_seat(7), Some(21));
        assert!(!ctx
            .key_generations
            .physically_held(HostId(10), EVDEV_KEY_BACKSPACE));
        assert!(!ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(10), EVDEV_KEY_BACKSPACE));
        assert!(ctx
            .key_generations
            .peek(HostId(10), EVDEV_KEY_BACKSPACE)
            .is_none());
        assert!(
            ctx.keyboard_repeatable_keys.contains_key(&HostId(10)),
            "focus teardown must preserve keymap-derived repeatability"
        );
        assert!(ctx
            .guest_key_owner(HostId(10), EVDEV_KEY_BACKSPACE)
            .is_none());
        assert!(!ctx
            .key_generations
            .host_accelerator_suppressed(HostId(10), EVDEV_KEY_BACKSPACE));
        assert!(!handler.modifiers.contains_key(&HostId(10)));
    }

    #[test]
    fn unmapped_leave_does_not_clear_live_keyboard_focus_state() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        map_keyboard(&mut ctx, 5, 10, 50, 7);
        ctx.shadow_table.map_id(20, 30);
        focus_keyboard(&mut ctx, 10, 7, 20);
        ctx.key_generations
            .observe_physical_state(HostId(10), EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        add_active_text_input(&mut ctx, 40, 7, 60);
        ctx.text_inputs
            .get_mut(&40)
            .expect("text input")
            .active_surface = Some(20);

        // Surface 999 was already unmapped from the guest, but the seat has a
        // live focused surface. A delayed leave for the old surface must not
        // tear down the current keyboard session.
        ctx.last_sender_id = 10;
        assert_eq!(handler.on_leave(&mut ctx, 1, 999), Action::Drop);
        assert!(ctx
            .key_generations
            .physically_held(HostId(10), EVDEV_KEY_BACKSPACE));
        assert_eq!(ctx.keyboard_focus.surface_for_seat(7), Some(20));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn matching_leave_uses_stored_host_surface_after_mapping_is_unavailable() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);
        map_keyboard(&mut ctx, 5, 10, 50, 7);
        add_active_text_input(&mut ctx, 40, 7, 60);
        ctx.keyboard_focus.set_for_test(HostId(10), 7, 20, 999);
        ctx.text_inputs
            .get_mut(&40)
            .expect("text input")
            .active_surface = Some(20);

        // The guest mapping is unavailable, but the registry retained the
        // exact host surface generation needed to match this leave.
        ctx.last_sender_id = 10;
        assert_eq!(handler.on_leave(&mut ctx, 1, 999), Action::Drop);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(7), None);
        assert!(ctx.text_inputs[&40].active_surface.is_none());
        assert!(!ctx.text_inputs[&40].host_is_active());
    }

    #[test]
    fn delayed_enter_for_pending_destroy_surface_does_not_revive_focus() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);

        // Model a live keyboard and a surface whose guest destroy request has
        // already been queued. The shadow mapping is intentionally retained
        // until host wl_display.delete_id so that a delayed host event can
        // still be translated; that event must not make the dead surface
        // current again.
        map_keyboard(&mut ctx, 5, 10, 50, 7);
        ctx.shadow_table.map_id(20, 30);
        ctx.shadow_table
            .track_interface(20, "wl_surface".to_string());
        ctx.shadow_table.mark_pending_destroy(20);
        assert!(ctx.shadow_table.is_pending_destroy_guest(20));

        ctx.last_sender_id = 10;
        assert_eq!(
            handler.on_enter(&mut ctx, 1, 30, &[]),
            Action::Drop,
            "the stale event must not be forwarded to the guest"
        );

        assert!(
            ctx.keyboard_focus.focus_for_keyboard(HostId(10)).is_none(),
            "a delayed enter must not register a destroyed surface on the keyboard"
        );
        assert!(
            ctx.keyboard_focus.surface_for_seat(7).is_none(),
            "a delayed enter must not revive seat IME focus for a destroyed surface"
        );
    }

    #[test]
    fn enter_without_keyboard_route_has_no_extension_side_effects() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_keyboard = 5;
        let host_keyboard = 10;

        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table.map_id(20, 30);
        ctx.host_keyboard_extension_id = Some(HostId(99));
        ctx.shadow_table.track_host_interface_with_version(
            99,
            "zcr_keyboard_extension_v1".to_string(),
            2,
        );

        ctx.last_sender_id = host_keyboard;
        assert_eq!(
            handler.on_enter(&mut ctx, 1, 30, &[]),
            Action::Drop,
            "an enter without a guest keyboard-to-seat route is undeliverable"
        );
        assert!(ctx.keyboard_to_extended_keyboard.is_empty());
        assert!(ctx.extended_keyboard_to_keyboard.is_empty());
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(ctx
            .keyboard_focus
            .focus_for_keyboard(HostId(host_keyboard))
            .is_none());
    }

    #[test]
    fn new_focus_retires_stale_keyboard_state_before_delayed_leave() {
        let mut handler = KeyboardHandler::new();
        let mut ctx = Context::new_for_test(false, false, vec![]);

        // Keyboard A held Backspace on a surface that was destroyed. Keyboard
        // B subsequently owns the same seat and a live surface. A's delayed
        // leave is unmapped; it must retire A's per-keyboard state without
        // disturbing B's focus or physical key state.
        map_keyboard(&mut ctx, 5, 10, 50, 7);
        map_keyboard(&mut ctx, 6, 11, 51, 7);
        ctx.shadow_table.map_id(20, 30);
        ctx.shadow_table.map_id(21, 31);
        focus_keyboard(&mut ctx, 10, 7, 21);
        ctx.key_generations
            .observe_physical_state(HostId(10), EVDEV_KEY_BACKSPACE, WL_KEY_PRESSED);
        ctx.key_generations
            .cancel_backspace_repeat(HostId(10), EVDEV_KEY_BACKSPACE);
        assert!(ctx.claim_guest_key(HostId(10), EVDEV_KEY_BACKSPACE, GuestKeyOwner::ImeRecovery));

        ctx.last_sender_id = 11;
        assert_eq!(handler.on_enter(&mut ctx, 1, 30, &[]), Action::Forward);
        ctx.last_sender_id = 10;
        assert_eq!(handler.on_leave(&mut ctx, 2, 999), Action::Drop);

        assert!(
            !ctx.key_generations
                .physically_held(HostId(10), EVDEV_KEY_BACKSPACE),
            "stale keyboard A pressed state must be retired"
        );
        assert!(
            !ctx.key_generations
                .backspace_repeat_cancelled(HostId(10), EVDEV_KEY_BACKSPACE),
            "stale keyboard A repeat marker must be retired"
        );
        assert!(
            ctx.guest_key_owner(HostId(10), EVDEV_KEY_BACKSPACE)
                .is_none(),
            "stale keyboard A IME marker must be retired"
        );
        assert!(
            ctx.keyboard_focus.keyboard_owns_surface(HostId(11), 20),
            "keyboard B's focus must remain active"
        );
        assert_eq!(ctx.keyboard_focus.surface_for_seat(7), Some(20));
        assert!(
            !crate::handler::text_input::backspace_pressed_for_seat(&ctx, 7),
            "a dead keyboard must not keep the seat's Backspace fallback armed"
        );
    }
}
