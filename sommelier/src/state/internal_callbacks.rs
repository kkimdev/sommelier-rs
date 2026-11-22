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

//! Host-only `wl_display.sync` callbacks the proxy issues for itself.
//!
//! Each one is an ordered host-stream barrier: its `wl_callback.done`
//! proves that every event caused by an earlier request has been delivered.
//! The guest never sees these callbacks. [`InternalCallbacks`] records what
//! each one waits for, so `wl_callback.done` dispatches through one match
//! in `crate::handler::callback`.

use std::collections::BTreeMap;

use super::{GuestId, HostId};
use crate::ime::BarrierToken;

/// What an internal callback waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InternalCallback {
    /// Every legacy format/modifier event of the internal linux-dmabuf
    /// binding of host global `global` has arrived.
    DmabufCapabilities { global: u32 },
    /// The internal `zaura_shell` bind has reached the compositor, so the
    /// synthetic `gtk_shell1` (guest ID) may announce its capabilities.
    GtkShellCapabilities { gtk_shell: GuestId },
    /// A host text input has delivered every event it sent before the
    /// request this barrier of the IME core follows: the `deactivate` of
    /// its drain, or the `reset` of a press's fence.
    TextInput { token: BarrierToken },
    /// The host has sent every event of the task that sent an empty IME
    /// confirmation, whose recovery of a held key waits
    /// (`crate::keys::recovery`). The `done` decides
    /// it, as any host event that comes before it would.
    KeyRecovery,
    /// The host has sent each registry the guest created before this sync
    /// every global it had when the registry was created, so a guest
    /// `wl_display.sync` that waits for the guest's globals
    /// (`crate::handler::registry::globals_settled`) may know them all.
    RegistryGlobals,
    /// Exo has handled a placement step of the toplevel of this host
    /// `zaura_toplevel` (`crate::handler::window_placement`), and sent the
    /// configures it causes.
    PlacementBarrier { zaura_toplevel: HostId },
}

/// One connection's pending internal callbacks, keyed by host callback ID.
///
/// A callback stays here from its `wl_display.sync` until its terminal
/// `done`, even when the state it waits for is gone, so its host ID is
/// never mistaken for a guest callback.
#[derive(Debug, Default)]
pub(crate) struct InternalCallbacks {
    by_callback: BTreeMap<HostId, InternalCallback>,
}

impl InternalCallbacks {
    /// Record what a newly issued callback waits for. Its host ID was just
    /// allocated, so it cannot be pending already.
    pub(crate) fn register(&mut self, callback: HostId, waits_for: InternalCallback) {
        let previous = self.by_callback.insert(callback, waits_for);
        debug_assert!(previous.is_none(), "a pending callback ID was reused");
    }

    /// Retire a callback at its terminal `done`.
    ///
    /// # Returns
    ///
    /// What it waited for, or `None` when `callback` is not internal.
    pub(crate) fn complete(&mut self, callback: HostId) -> Option<InternalCallback> {
        self.by_callback.remove(&callback)
    }

    /// Whether a pending callback still collects the capabilities of host
    /// linux-dmabuf global `global`, which therefore must not be reclaimed.
    pub(crate) fn awaits_dmabuf_capabilities(&self, global: u32) -> bool {
        self.by_callback
            .values()
            .any(|waits_for| *waits_for == InternalCallback::DmabufCapabilities { global })
    }

    /// Whether a pending callback waits for a registry's globals
    /// ([`InternalCallback::RegistryGlobals`]).
    pub(crate) fn awaits_registry_globals(&self) -> bool {
        self.by_callback
            .values()
            .any(|waits_for| *waits_for == InternalCallback::RegistryGlobals)
    }

    /// The pending callback that waits for `waits_for`, if any.
    #[cfg(test)]
    pub(crate) fn awaits(&self, waits_for: InternalCallback) -> Option<HostId> {
        self.by_callback
            .iter()
            .find_map(|(&callback, &pending)| (pending == waits_for).then_some(callback))
    }

    #[cfg(test)]
    pub(crate) fn get(&self, callback: HostId) -> Option<InternalCallback> {
        self.by_callback.get(&callback).copied()
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.by_callback.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_is_pending_from_register_until_complete() {
        let mut callbacks = InternalCallbacks::default();
        let waits_for = InternalCallback::DmabufCapabilities { global: 7 };
        callbacks.register(HostId::for_test(40), waits_for);
        assert_eq!(callbacks.get(HostId::for_test(40)), Some(waits_for));
        assert!(callbacks.awaits_dmabuf_capabilities(7));
        assert!(!callbacks.awaits_dmabuf_capabilities(8));

        assert_eq!(callbacks.complete(HostId::for_test(40)), Some(waits_for));
        assert_eq!(
            callbacks.complete(HostId::for_test(40)),
            None,
            "done is terminal"
        );
        assert!(!callbacks.awaits_dmabuf_capabilities(7));
        assert_eq!(callbacks.count(), 0);
    }
}
