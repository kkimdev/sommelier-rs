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

//! A host keyboard's keymap: compiling the one `wl_keyboard.keymap`
//! carries ([`compile`]), and the XKB state the host's modifiers keep in it
//! ([`Keymap`]), which classifies its keys.

use std::collections::BTreeSet;
use std::os::fd::{AsRawFd, BorrowedFd};

use crate::accelerator::Accelerator;
use crate::protocols::wayland::wl_keyboard::{KEYMAP_FORMAT_NO_KEYMAP, KEYMAP_FORMAT_XKB_V1};
use xkbcommon::xkb;

/// The largest `wl_keyboard.keymap` the proxy maps and compiles, in bytes.
/// Exo's keymaps are about 60 KiB. The size of a VirtWL keymap cannot be
/// checked against its file, and the proxy copies the keymap to compile it,
/// so a host size near 4 GiB would otherwise abort the process.
pub(crate) const MAX_KEYMAP_SIZE: u32 = 1 << 20;

/// The modifier and group state a host `wl_keyboard.modifiers` event
/// carries, as XKB masks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KeyboardModifiers {
    pub(crate) depressed: u32,
    pub(crate) latched: u32,
    pub(crate) locked: u32,
    pub(crate) group: u32,
}

/// A host keyboard's compiled keymap, the XKB state the host's modifiers
/// keep in it, and the keys it repeats, which all come from it.
pub(crate) struct Keymap {
    state: xkb::State,
    /// The evdev keycodes the keymap repeats.
    repeatable: BTreeSet<u32>,
}

impl std::fmt::Debug for Keymap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keymap")
            .field("repeatable", &self.repeatable.len())
            .finish_non_exhaustive()
    }
}

impl Keymap {
    /// `keymap`, under `modifiers`: Exo sends only the keymap on a layout
    /// change, so the modifiers held and locked before it still apply.
    pub(crate) fn new(keymap: &xkb::Keymap, modifiers: KeyboardModifiers) -> Self {
        let repeatable = (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .filter_map(|keycode_raw| {
                (keycode_raw >= 8 && keymap.key_repeats(xkb::Keycode::new(keycode_raw)))
                    .then_some(keycode_raw - 8)
            })
            .collect();
        let mut keymap = Self {
            state: xkb::State::new(keymap),
            repeatable,
        };
        keymap.set_modifiers(modifiers);
        keymap
    }

    /// Apply the host's `modifiers`.
    pub(crate) fn set_modifiers(&mut self, modifiers: KeyboardModifiers) {
        self.state.update_mask(
            modifiers.depressed,
            modifiers.latched,
            modifiers.locked,
            0,
            0,
            modifiers.group,
        );
    }

    /// Whether the keymap repeats evdev key `key`.
    pub(crate) fn repeats(&self, key: u32) -> bool {
        self.repeatable.contains(&key)
    }

    /// The chord `key` forms under the current state
    /// ([`Accelerator::of_key`]).
    pub(crate) fn chord(&self, key: u32) -> Option<Accelerator> {
        Accelerator::of_key(&self.state, key)
    }

    /// Whether `key` is a modifier in the keymap, which edits no text.
    pub(crate) fn is_modifier(&self, key: u32) -> bool {
        let Some(xkb_raw_keycode) = key.checked_add(8) else {
            return false;
        };
        let keycode = xkb::Keycode::new(xkb_raw_keycode);
        let keymap = self.state.get_keymap();
        let syms = keymap.key_get_syms_by_level(keycode, 0, 0);
        !syms.is_empty() && syms.iter().all(|&sym| is_modifier_keysym(sym.raw()))
    }

    /// The XKB state, for tests that read it.
    #[cfg(test)]
    pub(crate) fn state(&self) -> &xkb::State {
        &self.state
    }

    /// The keys the keymap repeats, for tests that read them.
    #[cfg(test)]
    pub(crate) fn repeatable(&self) -> &BTreeSet<u32> {
        &self.repeatable
    }
}

/// The default XKB keymap, as a host keyboard of a test sends it.
#[cfg(test)]
pub(crate) fn default_for_test() -> xkb::Keymap {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
    xkb::Keymap::new_from_names(&context, "", "", "", "", None, flags).expect("the default keymap")
}

/// X11's `IsModifierKey`: Shift to Hyper, the ISO lock and level keys,
/// Mode_switch and Num_Lock.
fn is_modifier_keysym(sym: u32) -> bool {
    use xkb::keysyms::*;
    (KEY_Shift_L..=KEY_Hyper_R).contains(&sym)
        || (KEY_ISO_Lock..=KEY_ISO_Level5_Lock).contains(&sym)
        || sym == KEY_Mode_switch
        || sym == KEY_Num_Lock
}

/// The XKB context a connection compiles its host keyboards' keymaps in.
/// It is `!Send` and `!Sync`, like the keymaps and states compiled from it.
pub(crate) struct Compiler {
    context: xkb::Context,
}

impl std::fmt::Debug for Compiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Compiler")
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        }
    }
}

impl Compiler {
    /// Compile the keymap of `format` and `size` that a
    /// `wl_keyboard.keymap` carries in `fd`, or log why the proxy cannot use
    /// it and return `None`. The keymap is mapped rather than read, so the
    /// descriptor's offset is untouched when it is forwarded to the guest.
    pub(crate) fn compile(
        &self,
        format: u32,
        fd: BorrowedFd<'_>,
        size: u32,
    ) -> Option<xkb::Keymap> {
        compile(&self.context, format, fd, size)
    }
}

fn compile(
    context: &xkb::Context,
    format: u32,
    fd: BorrowedFd<'_>,
    size: u32,
) -> Option<xkb::Keymap> {
    if format == KEYMAP_FORMAT_NO_KEYMAP {
        return None;
    }
    if format != KEYMAP_FORMAT_XKB_V1 {
        log::warn!("on_keymap: unsupported keymap format {}, ignoring", format);
        return None;
    }
    // A zero-size keymap is malformed; mmap(len=0) is UB per POSIX.
    if size == 0 {
        log::warn!("on_keymap: received zero-size keymap from host, ignoring");
        return None;
    }
    if size > MAX_KEYMAP_SIZE {
        log::warn!(
            "on_keymap: keymap of {size} bytes exceeds the {MAX_KEYMAP_SIZE}-byte limit, ignoring"
        );
        return None;
    }

    // size is a u32 from the Wayland wire; the cast to usize is lossless on
    // 64-bit Linux (the only supported target for sommelier).
    let Some(mapping) = MmapView::from_fd(fd, size as usize) else {
        log::error!(
            "on_keymap: mmap failed for fd={}, size={}",
            fd.as_raw_fd(),
            size
        );
        return None;
    };
    // Per the Wayland spec, wl_keyboard.keymap.size includes a trailing NUL.
    let Some((&0, text)) = mapping.as_bytes().split_last() else {
        log::error!(
            "on_keymap: keymap data is missing the trailing NUL required by the Wayland spec"
        );
        return None;
    };
    let text = match std::str::from_utf8(text) {
        Ok(text) => text,
        Err(e) => {
            log::error!("on_keymap: keymap bytes are not valid UTF-8: {}", e);
            return None;
        }
    };
    // xkbcommon takes the text as a C string, and panics on a NUL inside.
    if text.contains('\0') {
        log::error!("on_keymap: keymap text holds a NUL before its end");
        return None;
    }
    let keymap = xkb::Keymap::new_from_string(
        context,
        // `new_from_string` takes ownership; copy required because `text`
        // borrows from the mmap region, which is unmapped when `mapping`
        // drops at the end of this function.
        text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    );
    if keymap.is_none() {
        log::error!("on_keymap: xkbcommon failed to compile the keymap string");
    }
    keymap
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

    /// The mapping's address, for tests that find it in the process's
    /// memory map.
    #[cfg(test)]
    fn address(&self) -> usize {
        self.ptr.as_ptr() as usize
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

#[cfg(test)]
mod tests;
