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

//! Lifecycle of the host-only Aura children the proxy creates from its
//! internal `zaura_shell`: `zaura_surface`, `zaura_toplevel` and
//! `zaura_output`.
//!
//! The guest never sees these objects. This module is the one place that
//! knows how they are created, which version they have, and how they are
//! released; the features that use them keep the associations:
//!
//! | Child | Association owner | Used by |
//! | :--- | :--- | :--- |
//! | `zaura_surface` | [`crate::state::AuraSurfaces`] | XDG and GTK metadata, placement |
//! | `zaura_toplevel` | [`crate::state::PlacementState`] | placement |
//! | `zaura_output` | [`crate::state::OutputRegistry`] | placement metadata |

use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
use crate::protocols::Iface;
use crate::state::{Context, HostAuraShell};
use crate::wire::{queue_or_drop, Outgoing};

/// What the proxy needs to know about one kind of host Aura child.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AuraChild {
    interface: Iface,
    /// The `zaura_shell` request that creates the child, from the shell,
    /// the new child's ID and the object it extends.
    get: fn(u32, u32, u32) -> Outgoing,
    /// The child's destructor, which exists from `release_since` on.
    release: fn(u32) -> Outgoing,
    release_since: u32,
}

impl AuraChild {
    pub(crate) const SURFACE: Self = Self {
        interface: Iface::ZauraSurface,
        get: zaura_shell::req::get_aura_surface,
        release: zaura_surface::req::release,
        release_since: zaura_surface::REQ_RELEASE_SINCE,
    };
    pub(crate) const TOPLEVEL: Self = Self {
        interface: Iface::ZauraToplevel,
        get: zaura_shell::req::get_aura_toplevel_for_xdg_toplevel,
        release: zaura_toplevel::req::release,
        release_since: zaura_toplevel::REQ_RELEASE_SINCE,
    };
    pub(crate) const OUTPUT: Self = Self {
        interface: Iface::ZauraOutput,
        get: zaura_shell::req::get_aura_output,
        release: zaura_output::req::release,
        release_since: zaura_output::REQ_RELEASE_SINCE,
    };
}

/// The version [`create`] recorded for `child_host_id` while it is a live
/// Aura child of kind `child`: its shell's.
///
/// `None` when it is not one: the ID is retired or untracked, or was
/// recycled for an object of another interface, whose requests have other
/// opcodes. Association owners forget a child before it ends, so that is a
/// bookkeeping bug, which debug builds assert against.
fn live_child_version(ctx: &Context, child: AuraChild, child_host_id: u32) -> Option<u32> {
    let version = (ctx.shadow_table.get_host_interface(child_host_id) == Some(child.interface))
        .then(|| ctx.shadow_table.host_object_version(child_host_id))
        .flatten();
    debug_assert!(
        version.is_some(),
        "host ID {child_host_id} is not a live {}",
        child.interface
    );
    version
}

/// Create a host Aura child that extends `parent_host_id`.
///
/// Allocates a host-only ID, records it with the shell's version, and queues
/// the `zaura_shell` request that creates it. The caller records the
/// returned ID with the child's association owner.
///
/// Every bound shell can create Aura surfaces and outputs (see the registry's
/// `ZAURA_SHELL_MIN_VERSION`); the one caller that creates a toplevel checks
/// the newer version placement needs first.
///
/// # Returns
///
/// `None`, queueing nothing, when no `zaura_shell` is bound.
pub(crate) fn create(ctx: &mut Context, child: AuraChild, parent_host_id: u32) -> Option<u32> {
    let shell = ctx.host_zaura_shell?;
    let child_host_id = ctx
        .shadow_table
        .create_host_only(child.interface, shell.version);
    ctx.client_to_host_queue
        .push((child.get)(shell.id, child_host_id, parent_host_id));
    Some(child_host_id)
}

/// Release a host Aura child whose association owner has forgotten it (see
/// [`release_host_only`]).
///
/// Callers release a child before they queue the destructor of the object
/// it extends, so the host never holds an Aura child whose parent is gone.
///
/// An ID that is not a live child of kind `child` (see
/// [`live_child_version`]) is left alone: a retired or untracked one has
/// nothing left to release, and a recycled one belongs to another object.
pub(crate) fn release(ctx: &mut Context, child: AuraChild, child_host_id: u32) {
    let Some(version) = live_child_version(ctx, child, child_host_id) else {
        return;
    };
    release_host_only(
        ctx,
        child_host_id,
        version,
        child.release,
        child.release_since,
    );
}

/// Release the internal `zaura_shell` binding after its `global_remove`
/// (see [`release_host_only`]). Its children stay usable.
pub(crate) fn release_shell(ctx: &mut Context, shell: HostAuraShell) {
    release_host_only(
        ctx,
        shell.id,
        shell.version,
        zaura_shell::req::release,
        zaura_shell::REQ_RELEASE_SINCE,
    );
}

/// Release a host-only Aura object of negotiated `version`.
///
/// From the `release_since` version on, the destructor is queued and the
/// host ID stays reserved until the compositor acknowledges it with
/// `wl_display.delete_id`, so a recycled ID never receives the late
/// acknowledgement. Older objects have no destructor: their dispatch
/// metadata is retired so stale host events cannot reach them, and the
/// numeric ID stays reserved for the rest of the connection.
fn release_host_only(
    ctx: &mut Context,
    host_id: u32,
    version: u32,
    release: fn(u32) -> Outgoing,
    release_since: u32,
) {
    if version >= release_since {
        ctx.client_to_host_queue.push(release(host_id));
        ctx.retire_destroyed_host_object(host_id);
    } else {
        ctx.shadow_table.retire_host(host_id);
    }
}

/// Create or reuse the one host Aura surface of a guest wl_surface (see
/// [`crate::state::AuraSurfaces`]).
///
/// # Returns
///
/// `None` when the wl_surface has no host object or no `zaura_shell` is
/// bound, and, in release builds, when the Aura surface recorded for it is
/// no longer live.
pub(crate) fn ensure_host_zaura_surface(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
) -> Option<u32> {
    let wl_surface_host_id = ctx.shadow_table.get_host_id(wl_surface_guest_id)?;
    if let Some(zaura_surface_host_id) = ctx.aura_surfaces.get(wl_surface_host_id) {
        // A recorded surface that is no longer live is a bookkeeping bug
        // (see `live_child_version`), and nothing may be sent to it.
        return live_child_version(ctx, AuraChild::SURFACE, zaura_surface_host_id)
            .map(|_| zaura_surface_host_id);
    }
    let zaura_surface_host_id = create(ctx, AuraChild::SURFACE, wl_surface_host_id)?;
    ctx.aura_surfaces
        .remember(wl_surface_host_id, zaura_surface_host_id);
    Some(zaura_surface_host_id)
}

/// Send the application ID the guest set (through XDG or GTK) to the
/// surface's host Aura surface, creating that surface if needed. Nothing is
/// sent while no `zaura_shell` is bound.
///
/// The Aura surface needs no version check: the registry binds no
/// `zaura_shell` older than `set_application_id`.
///
/// # Arguments
///
/// * `native_application_id` - the guest's ID in the Guest OS namespace
///   (`compositor::native_wayland_app_id`). An ID too long for one message
///   is dropped with a warning.
pub(crate) fn set_application_id(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
    native_application_id: &str,
) {
    let Some(zaura_surface_host_id) = ensure_host_zaura_surface(ctx, wl_surface_guest_id) else {
        return;
    };
    if queue_or_drop(
        &mut ctx.client_to_host_queue,
        zaura_surface::req::set_application_id(zaura_surface_host_id, Some(native_application_id)),
        format_args!("the application ID of zaura_surface {zaura_surface_host_id}"),
    ) {
        log::debug!(
            "Set application ID to {} on zaura_surface (host_id={})",
            native_application_id,
            zaura_surface_host_id
        );
    }
}

/// Release the Aura surface of a host wl_surface that is being destroyed,
/// if it has one.
pub(crate) fn release_host_zaura_surface(ctx: &mut Context, wl_surface_host_id: u32) {
    if let Some(zaura_surface_host_id) = ctx.aura_surfaces.take(wl_surface_host_id) {
        release(ctx, AuraChild::SURFACE, zaura_surface_host_id);
    }
}

/// Give a live output its host-only `zaura_output` if it has none yet.
///
/// It carries the Aura scale factors, logical transform and work-area insets
/// that placement needs; the guest only sees the plain `wl_output`. Without a
/// bound `zaura_shell` nothing is created, and binding one retries.
pub(crate) fn ensure_zaura_output(ctx: &mut Context, wl_output_host_id: u32) {
    if ctx.outputs.aura_output(wl_output_host_id).is_some()
        || !ctx.outputs.is_live(wl_output_host_id)
    {
        return;
    }
    if let Some(zaura_output_host_id) = create(ctx, AuraChild::OUTPUT, wl_output_host_id) {
        ctx.outputs
            .remember_aura_output(wl_output_host_id, zaura_output_host_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::queued_headers;

    const SHELL_HOST: u32 = 0xff00_0001;
    const PARENT_HOST: u32 = 0xff00_0002;
    /// Each child with the opcodes of its `zaura_shell` request and its
    /// destructor.
    const ALL: [(AuraChild, u16, u16); 3] = [
        (
            AuraChild::SURFACE,
            zaura_shell::REQ_GET_AURA_SURFACE,
            zaura_surface::REQ_RELEASE,
        ),
        (
            AuraChild::TOPLEVEL,
            zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
            zaura_toplevel::REQ_RELEASE,
        ),
        (
            AuraChild::OUTPUT,
            zaura_shell::REQ_GET_AURA_OUTPUT,
            zaura_output::REQ_RELEASE,
        ),
    ];

    fn ctx_with_shell(version: u32) -> Context {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_zaura_shell = Some(crate::state::HostAuraShell {
            id: SHELL_HOST,
            global_name: SHELL_HOST,
            version,
        });
        ctx
    }

    #[test]
    fn create_queues_the_shell_request_with_the_shell_version() {
        // The oldest shell that can create every child.
        let version = zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE;
        for (child, get_opcode, _) in ALL {
            let mut ctx = ctx_with_shell(version);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            assert_eq!(
                queued_headers(&ctx.client_to_host_queue),
                vec![(SHELL_HOST, get_opcode)]
            );
            assert_eq!(
                ctx.client_to_host_queue[0].0[8..16],
                [id.to_ne_bytes(), PARENT_HOST.to_ne_bytes()].concat()
            );
            assert_eq!(
                ctx.shadow_table.get_host_interface(id),
                Some(child.interface)
            );
            assert_eq!(live_child_version(&ctx, child, id), Some(version));
        }
    }

    #[test]
    fn create_needs_a_bound_shell() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        assert_eq!(create(&mut ctx, AuraChild::SURFACE, PARENT_HOST), None);
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn release_follows_each_childs_release_version() {
        for (child, _, release_opcode) in ALL {
            let since = child.release_since;
            let mut ctx = ctx_with_shell(since);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            ctx.client_to_host_queue.clear();
            release(&mut ctx, child, id);
            assert_eq!(
                queued_headers(&ctx.client_to_host_queue),
                vec![(id, release_opcode)]
            );
            assert!(ctx.shadow_table.is_retired_host(id));

            let mut ctx = ctx_with_shell(since - 1);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            ctx.client_to_host_queue.clear();
            release(&mut ctx, child, id);
            assert!(
                ctx.client_to_host_queue.is_empty(),
                "{child:?} has no destructor"
            );
            assert_eq!(ctx.shadow_table.get_host_interface(id), None);
            assert!(ctx.shadow_table.is_retired_host(id));
            assert!(!ctx.shadow_table.is_host_id_available(id));
        }
    }

    #[test]
    fn only_a_live_output_without_one_gets_an_aura_output() {
        let mut ctx = ctx_with_shell(crate::protocols::aura_shell::zaura_shell::VERSION);
        ctx.outputs
            .bind_for_test(PARENT_HOST, crate::state::OutputState::default());
        ensure_zaura_output(&mut ctx, PARENT_HOST);
        let aura_output = ctx.outputs.aura_output(PARENT_HOST).expect("Aura output");
        ensure_zaura_output(&mut ctx, PARENT_HOST);
        assert_eq!(ctx.client_to_host_queue.len(), 1, "an output gets one");
        assert_eq!(ctx.outputs.aura_output(PARENT_HOST), Some(aura_output));

        let removed = PARENT_HOST + 1;
        ctx.outputs
            .bind_for_test(removed, crate::state::OutputState::default());
        let removed_aura_outputs = ctx.outputs.remove_global(crate::state::GlobalBinding {
            name: removed,
            generation: 1,
        });
        assert!(removed_aura_outputs.is_empty());
        ctx.client_to_host_queue.clear();
        ensure_zaura_output(&mut ctx, removed);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(ctx.outputs.aura_output(removed), None);
    }
}
