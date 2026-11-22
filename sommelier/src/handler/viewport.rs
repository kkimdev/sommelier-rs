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

//! The guest's `wp_viewporter` and `wp_viewport`: each viewport's link to
//! its `wl_surface`, and the crop and scale its requests set as the
//! surface's pending state, which the surface's next commit applies
//! (`SurfaceState::pending_viewport_mut`).

use crate::handler::Handlers;
use crate::protocols::viewporter::{wp_viewport, wp_viewporter};
use crate::state::{Context, GuestId, PostDispatch, RequestObject, RequestSender};
use crate::wire::Action;

/// A guest `wp_viewport`; its table entry keeps it.
#[derive(Debug)]
pub(crate) struct Viewport {
    /// The `wl_surface` it scales, until that ends: then its requests
    /// raise `no_surface`, and a later surface under the ID is not its.
    surface: Option<GuestId>,
}

#[cfg(test)]
impl Viewport {
    /// The surface it scales, for the orphan check.
    pub(crate) fn surface_for_test(&self) -> Option<GuestId> {
        self.surface
    }
}

/// The surface of live viewport `viewport`, or the error a request of one
/// that has none ends the session with.
fn viewport_surface(ctx: &mut Context, viewport: GuestId) -> Result<GuestId, Action> {
    let surface = ctx.shadow_table.data::<Viewport>(viewport);
    surface
        .and_then(|viewport| viewport.surface)
        .ok_or_else(|| {
            ctx.reject(
                viewport,
                wp_viewport::ERROR_NO_SURFACE,
                "wp_viewport has no associated wl_surface",
            )
        })
}

impl wp_viewporter::WpViewporterHandler for Handlers {
    fn on_get_viewport(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        surface: RequestObject,
    ) -> Action {
        let surface = surface.guest;
        if ctx
            .shadow_table
            .data_of::<Viewport>()
            .any(|(_, viewport)| viewport.surface == Some(surface))
        {
            log::warn!("Rejecting duplicate wp_viewport for wl_surface {}", surface);
            return ctx.reject(
                sender.guest,
                wp_viewporter::ERROR_VIEWPORT_EXISTS,
                "wl_surface already has a wp_viewport",
            );
        }
        let Some(state) = ctx.surface_mut(surface) else {
            return ctx.missing_surface_state(surface);
        };
        state.add_viewport();
        let viewport = Viewport {
            surface: Some(surface),
        };
        ctx.defer_until_forwarded(PostDispatch::Attach {
            object: id,
            data: viewport.into(),
        });
        Action::Forward
    }
}

impl wp_viewport::WpViewportHandler for Handlers {
    fn on_set_source(
        ctx: &mut Context,
        sender: RequestSender,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let surface_id = match viewport_surface(ctx, sender.guest) {
            Ok(surface) => surface,
            Err(action) => return action,
        };
        let unset = (x, y, width, height) == (-256, -256, -256, -256);
        if !unset && (x < 0 || y < 0 || width <= 0 || height <= 0) {
            log::warn!("Rejecting invalid wp_viewport source rectangle");
            return ctx.reject(
                sender.guest,
                wp_viewport::ERROR_BAD_VALUE,
                "invalid wp_viewport source",
            );
        }
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface.pending_viewport_mut().source = (!unset).then_some((x, y, width, height));
        Action::Forward
    }

    fn on_set_destination(
        ctx: &mut Context,
        sender: RequestSender,
        width: i32,
        height: i32,
    ) -> Action {
        let surface_id = match viewport_surface(ctx, sender.guest) {
            Ok(surface) => surface,
            Err(action) => return action,
        };
        let unset = width == -1 && height == -1;
        if !unset && (width <= 0 || height <= 0) {
            log::warn!(
                "Rejecting invalid wp_viewport destination {}x{}",
                width,
                height
            );
            return ctx.reject(
                sender.guest,
                wp_viewport::ERROR_BAD_VALUE,
                "invalid wp_viewport destination",
            );
        }
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface.pending_viewport_mut().destination = (!unset).then_some((width, height));
        Action::Forward
    }
}

/// Forget guest `wp_viewport` `viewport`, which ended (see
/// `crate::state::lifecycle`): its surface's next commit removes the
/// viewport.
pub(crate) fn forget_viewport(ctx: &mut Context, viewport: Viewport) {
    if let Some(surface) = viewport.surface.and_then(|id| ctx.surface_mut(id)) {
        surface.remove_viewport();
    }
}

/// Unlink the viewport of guest `wl_surface` `surface`, which ended (see
/// `crate::state::lifecycle`): its later requests raise `no_surface`.
pub(crate) fn forget_wl_surface(ctx: &mut Context, surface: GuestId) {
    for (_, viewport) in ctx.shadow_table.data_of_mut::<Viewport>() {
        if viewport.surface == Some(surface) {
            viewport.surface = None;
        }
    }
}
