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

//! The remote-shell-v2 placement backend's host objects.
//!
//! `zcr_remote_shell_v2` is bound host-side only and never advertised to
//! guests, so its objects receive host events alone. Nothing binds the
//! global yet; [`RemoteShellHandler`] receives the events of its objects.

use crate::protocols::remote_shell_unstable_v2::{
    self, zcr_remote_shell_v2, zcr_remote_surface_v2,
};

/// The host event handler of the host-only remote-shell objects. No guest
/// object pairs with one, so an event it forwards reaches no guest.
pub struct RemoteShellHandler;

impl remote_shell_unstable_v2::ProtocolHandler for RemoteShellHandler {}

impl zcr_remote_shell_v2::ZcrRemoteShellV2Handler for RemoteShellHandler {}

impl zcr_remote_surface_v2::ZcrRemoteSurfaceV2Handler for RemoteShellHandler {}

#[cfg(test)]
mod tests {
    use crate::protocols::remote_shell_unstable_v2::{zcr_remote_shell_v2, zcr_remote_surface_v2};

    /// Wayland opcodes are positional, and the vendored XML is trimmed:
    /// these are Exo's indices (`zcr_remote_shell_v2.cc`), which a dropped
    /// or reordered message would shift.
    #[test]
    fn vendored_remote_shell_keeps_upstream_opcodes() {
        assert_eq!(zcr_remote_shell_v2::VERSION, 6);
        assert_eq!(zcr_remote_shell_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE, 1);
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_OUTPUT, 5);
        assert_eq!(zcr_remote_shell_v2::EVT_DESKTOP_FOCUS_STATE_CHANGED, 2);
        assert_eq!(zcr_remote_surface_v2::VERSION, 6);
        assert_eq!(zcr_remote_surface_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_APP_ID, 1);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_TITLE, 2);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT, 37);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR, 40);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR_SINCE, 4);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SHADOW_CORNER_RADII, 42);
        assert_eq!(zcr_remote_surface_v2::EVT_CLOSE, 0);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED, 3);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED_IN_OUTPUT, 7);
    }
}
