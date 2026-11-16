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
//! | `zaura_toplevel` | [`crate::state::WindowPlacementState`] | placement |
//! | `zaura_output` | [`crate::state::OutputRegistry`] | placement metadata |

use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
use crate::state::Context;
use crate::wire::MessageBuilder;

/// What the proxy needs to know about one kind of host Aura child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AuraChild {
    interface: &'static str,
    /// The `zaura_shell` request that creates the child. Each takes the new
    /// child's ID followed by the object it extends.
    get_opcode: u16,
    /// The child's destructor, which exists from `release_since` on.
    release_opcode: u16,
    release_since: u32,
}

impl AuraChild {
    pub(crate) const SURFACE: Self = Self {
        interface: "zaura_surface",
        get_opcode: zaura_shell::REQ_GET_AURA_SURFACE,
        release_opcode: zaura_surface::REQ_RELEASE,
        release_since: zaura_surface::REQ_RELEASE_SINCE,
    };
    pub(crate) const TOPLEVEL: Self = Self {
        interface: "zaura_toplevel",
        get_opcode: zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
        release_opcode: zaura_toplevel::REQ_RELEASE,
        release_since: zaura_toplevel::REQ_RELEASE_SINCE,
    };
    pub(crate) const OUTPUT: Self = Self {
        interface: "zaura_output",
        get_opcode: zaura_shell::REQ_GET_AURA_OUTPUT,
        release_opcode: zaura_output::REQ_RELEASE,
        release_since: zaura_output::REQ_RELEASE_SINCE,
    };
}

/// The version [`create`] recorded for a live Aura child: its shell's.
/// A missing one is a bookkeeping bug; release builds then guess the
/// current shell's version (0 without one) rather than crash the proxy.
pub(crate) fn aura_object_version(ctx: &Context, aura_object_host_id: u32) -> u32 {
    let version = ctx.shadow_table.host_object_version(aura_object_host_id);
    debug_assert!(
        version.is_some(),
        "Aura child {aura_object_host_id} has no recorded version"
    );
    version
        .or(ctx.host_zaura_shell.map(|shell| shell.version))
        .unwrap_or_default()
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
    let child_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        child_host_id,
        child.interface.to_string(),
        shell.version,
    );
    let mut builder = MessageBuilder::new();
    builder.write_u32(child_host_id);
    builder.write_u32(parent_host_id);
    let message = builder.build_message(shell.id, child.get_opcode);
    ctx.client_to_host_queue.push((message, Vec::new()));
    Some(child_host_id)
}

/// Release a host Aura child whose association owner has forgotten it.
///
/// From the child's `release` since-version on, the destructor is queued and
/// the host ID stays reserved until the compositor acknowledges it with
/// `wl_display.delete_id`, so a recycled ID never receives the late
/// acknowledgement. Older children have no destructor: their dispatch
/// metadata is retired so stale host events cannot reach them, and the
/// numeric ID stays reserved for the rest of the connection.
///
/// Callers release a child before they queue the destructor of the object
/// it extends, so the host never holds an Aura child whose parent is gone.
pub(crate) fn release(ctx: &mut Context, child: AuraChild, child_host_id: u32) {
    if aura_object_version(ctx, child_host_id) >= child.release_since {
        let message = MessageBuilder::new().build_message(child_host_id, child.release_opcode);
        ctx.client_to_host_queue.push((message, Vec::new()));
        ctx.shadow_table.mark_pending_destroy_host(child_host_id);
    } else {
        ctx.shadow_table.retire_host_interface(child_host_id);
    }
}

/// Create or reuse the one host Aura surface of a guest wl_surface (see
/// [`crate::state::AuraSurfaces`]).
///
/// # Returns
///
/// `None` when the wl_surface has no host object or no `zaura_shell` is
/// bound.
pub(crate) fn ensure_host_zaura_surface(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
) -> Option<u32> {
    let wl_surface_host_id = ctx.shadow_table.get_host_id(wl_surface_guest_id)?;
    if let Some(zaura_surface_host_id) = ctx.aura_surfaces.get(wl_surface_host_id) {
        return Some(zaura_surface_host_id);
    }
    let zaura_surface_host_id = create(ctx, AuraChild::SURFACE, wl_surface_host_id)?;
    ctx.aura_surfaces
        .remember(wl_surface_host_id, zaura_surface_host_id);
    Some(zaura_surface_host_id)
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

    const SHELL_HOST: u32 = 0xff00_0001;
    const PARENT_HOST: u32 = 0xff00_0002;
    const ALL: [AuraChild; 3] = [AuraChild::SURFACE, AuraChild::TOPLEVEL, AuraChild::OUTPUT];

    fn ctx_with_shell(version: u32) -> Context {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_zaura_shell = Some(crate::state::HostAuraShell {
            id: SHELL_HOST,
            version,
        });
        ctx
    }

    fn queued(ctx: &Context) -> Vec<(u32, u16)> {
        ctx.client_to_host_queue
            .iter()
            .map(|(message, _)| {
                let sender = u32::from_ne_bytes(message[0..4].try_into().unwrap());
                let opcode = u32::from_ne_bytes(message[4..8].try_into().unwrap()) & 0xffff;
                (sender, opcode as u16)
            })
            .collect()
    }

    #[test]
    fn create_queues_the_shell_request_with_the_shell_version() {
        let version = 17;
        for child in ALL {
            let mut ctx = ctx_with_shell(version);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            assert_eq!(queued(&ctx), vec![(SHELL_HOST, child.get_opcode)]);
            assert_eq!(
                ctx.client_to_host_queue[0].0[8..16],
                [id.to_ne_bytes(), PARENT_HOST.to_ne_bytes()].concat()
            );
            assert_eq!(
                ctx.shadow_table.get_host_interface(id).map(String::as_str),
                Some(child.interface)
            );
            assert_eq!(aura_object_version(&ctx, id), version);
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
        for child in ALL {
            let since = child.release_since;
            let mut ctx = ctx_with_shell(since);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            ctx.client_to_host_queue.clear();
            release(&mut ctx, child, id);
            assert_eq!(queued(&ctx), vec![(id, child.release_opcode)]);
            assert!(ctx.shadow_table.is_pending_destroy_host_only(id));

            let mut ctx = ctx_with_shell(since - 1);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            ctx.client_to_host_queue.clear();
            release(&mut ctx, child, id);
            assert!(
                ctx.client_to_host_queue.is_empty(),
                "{child:?} has no destructor"
            );
            assert_eq!(ctx.shadow_table.get_host_interface(id), None);
            assert!(!ctx.shadow_table.is_pending_destroy_host_only(id));
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

        let inactive = PARENT_HOST + 1;
        ctx.outputs
            .bind_for_test(inactive, crate::state::OutputState::default());
        ctx.outputs.deactivate(crate::state::GlobalBinding {
            name: inactive,
            generation: 1,
        });
        ctx.client_to_host_queue.clear();
        ensure_zaura_output(&mut ctx, inactive);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(ctx.outputs.aura_output(inactive), None);
    }
}
