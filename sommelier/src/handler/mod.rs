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

pub mod aura_objects;
pub mod buffer;
pub mod callback;
pub mod compositor;
pub mod data_control;
pub mod data_device;
pub mod display;
pub mod gtk_shell;
pub mod keyboard;
pub mod linux_dmabuf;
pub mod output;
pub mod pointer;
pub mod registry;
pub mod seat;
pub mod security_context;
pub mod shm;
#[cfg(test)]
pub(crate) mod test_support;
pub mod text_input;
pub mod touch;
pub mod viewport;
pub mod window_placement;
pub mod xdg_shell;

/// The proxy's protocol handlers: one stateless type that implements the
/// generated handler trait of every interface. Each module implements the
/// traits of the interfaces it owns; the state they act on is the
/// connection's `Context`, which every method takes.
pub(crate) struct Handlers;

use crate::protocols::aura_shell::{zaura_popup, zaura_shell, zaura_surface};
use crate::protocols::fractional_scale_v1::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use crate::protocols::wayland::{wl_shell, wl_shell_surface};
use crate::protocols::xdg_decoration_unstable_v1::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use crate::protocols::xdg_output_unstable_v1::zxdg_output_v1;
use crate::protocols::xdg_shell::{xdg_popup, xdg_positioner};

// The interfaces whose messages all forward as they are.
impl wl_shell::WlShellHandler for Handlers {}
impl wl_shell_surface::WlShellSurfaceHandler for Handlers {}
impl xdg_positioner::XdgPositionerHandler for Handlers {}
impl xdg_popup::XdgPopupHandler for Handlers {}
impl zxdg_decoration_manager_v1::ZxdgDecorationManagerV1Handler for Handlers {}
impl zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1Handler for Handlers {}
impl wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1Handler for Handlers {}
impl wp_fractional_scale_v1::WpFractionalScaleV1Handler for Handlers {}
impl zxdg_output_v1::ZxdgOutputV1Handler for Handlers {}
// The Aura objects only the proxy uses, whose events reach no guest
// object.
impl zaura_shell::ZauraShellHandler for Handlers {}
impl zaura_surface::ZauraSurfaceHandler for Handlers {}
impl zaura_popup::ZauraPopupHandler for Handlers {}

/// Whether host event `event` of `sender`, whose guest object the guest
/// has destroyed, still reaches its handler, as the owner of the sender's
/// interface declares: it ends what the destroy left behind. Every other
/// event of a destroyed object reaches neither its handler nor the guest.
pub(crate) fn outlives_destroy(
    ctx: &crate::state::Context,
    sender: &crate::state::EventSender,
    event: &crate::protocols::AnyEvent,
) -> bool {
    use crate::protocols::AnyEvent;
    match event {
        AnyEvent::WlBuffer(event) => buffer::outlives_destroy(ctx, sender, event),
        AnyEvent::ZwpLinuxBufferParamsV1(event) => linux_dmabuf::outlives_destroy(event),
        _ => false,
    }
}
