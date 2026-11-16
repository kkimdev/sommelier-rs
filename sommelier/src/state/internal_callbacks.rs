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

use std::collections::HashMap;

use super::HostId;

/// What an internal callback waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InternalCallback {
    /// Every legacy format/modifier event of the internal linux-dmabuf
    /// binding of this capability `generation` has arrived.
    DmabufCapabilities { generation: u64 },
    /// The internal `zaura_shell` bind has reached the compositor, so the
    /// synthetic `gtk_shell1` (guest ID) may announce its capabilities.
    GtkShellCapabilities { gtk_shell: u32 },
    /// The reused host text-input-v1 object (`host_text_input`) of guest
    /// `text_input` has delivered every event of its previous activation.
    TextInputDrained {
        text_input: u32,
        host_text_input: u32,
    },
    /// Exo has processed the `set_window_bounds` that placement sent to this
    /// host `zaura_toplevel`.
    WindowBoundsBarrier { zaura_toplevel: u32 },
}

/// One connection's pending internal callbacks, keyed by host callback ID.
///
/// A callback stays here from its `wl_display.sync` until its terminal
/// `done`, even when the state it waits for is gone, so its host ID is
/// never mistaken for a guest callback.
#[derive(Debug, Default)]
pub(crate) struct InternalCallbacks {
    by_callback: HashMap<HostId, InternalCallback>,
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

    /// Whether a pending callback still collects this dmabuf capability
    /// generation, which therefore must not be reclaimed.
    pub(crate) fn awaits_dmabuf_generation(&self, generation: u64) -> bool {
        self.by_callback
            .values()
            .any(|waits_for| *waits_for == InternalCallback::DmabufCapabilities { generation })
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
        let waits_for = InternalCallback::DmabufCapabilities { generation: 7 };
        callbacks.register(HostId(40), waits_for);
        assert_eq!(callbacks.get(HostId(40)), Some(waits_for));
        assert!(callbacks.awaits_dmabuf_generation(7));
        assert!(!callbacks.awaits_dmabuf_generation(8));

        assert_eq!(callbacks.complete(HostId(40)), Some(waits_for));
        assert_eq!(callbacks.complete(HostId(40)), None, "done is terminal");
        assert!(!callbacks.awaits_dmabuf_generation(7));
        assert_eq!(callbacks.count(), 0);
    }
}
