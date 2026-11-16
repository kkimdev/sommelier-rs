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
use crate::state::{Context, ShadowTable};
use crate::wire::MessageBuilder;
use std::os::fd::RawFd;

/// A kind of host Aura child object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuraChild {
    Surface,
    Toplevel,
    Output,
}

/// A request's opcode and the version that introduced it.
struct VersionedRequest {
    opcode: u16,
    since: u32,
}

impl AuraChild {
    fn interface(self) -> &'static str {
        match self {
            Self::Surface => "zaura_surface",
            Self::Toplevel => "zaura_toplevel",
            Self::Output => "zaura_output",
        }
    }

    /// The `zaura_shell` request that creates this child. Each takes the new
    /// child's ID followed by the parent object it extends.
    const fn get_request(self) -> VersionedRequest {
        match self {
            Self::Surface => VersionedRequest {
                opcode: zaura_shell::REQ_GET_AURA_SURFACE,
                since: zaura_shell::REQ_GET_AURA_SURFACE_SINCE,
            },
            Self::Toplevel => VersionedRequest {
                opcode: zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
                since: zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE,
            },
            Self::Output => VersionedRequest {
                opcode: zaura_shell::REQ_GET_AURA_OUTPUT,
                since: zaura_shell::REQ_GET_AURA_OUTPUT_SINCE,
            },
        }
    }

    /// The child's destructor request. Before its since-version the child has
    /// no destructor at all.
    const fn release_request(self) -> VersionedRequest {
        match self {
            Self::Surface => VersionedRequest {
                opcode: zaura_surface::REQ_RELEASE,
                since: zaura_surface::REQ_RELEASE_SINCE,
            },
            Self::Toplevel => VersionedRequest {
                opcode: zaura_toplevel::REQ_RELEASE,
                since: zaura_toplevel::REQ_RELEASE_SINCE,
            },
            Self::Output => VersionedRequest {
                opcode: zaura_output::REQ_RELEASE,
                since: zaura_output::REQ_RELEASE_SINCE,
            },
        }
    }
}

/// Return the negotiated version of a host Aura child object.
///
/// Aura children take the version of the `zaura_shell` that created them,
/// and [`create`] records it. Every caller passes a live child, so a missing
/// version is a bookkeeping bug and fails the debug assertion. Release
/// builds fall back to the current shell version, the version the child
/// would have been created with: at worst that sends or skips one
/// version-gated request, which is better than crashing every client of
/// the proxy.
pub(crate) fn aura_object_version(ctx: &Context, aura_object_host_id: u32) -> u32 {
    let version = ctx.shadow_table.host_object_version(aura_object_host_id);
    debug_assert!(
        version.is_some(),
        "Aura child {aura_object_host_id} has no recorded version"
    );
    version.unwrap_or(ctx.host_zaura_shell_version)
}

/// Create a host Aura child that extends `parent_host_id`.
///
/// Allocates a host-only ID, records it with the shell's version, and queues
/// the `zaura_shell` request that creates it. The caller records the
/// returned ID with the child's association owner.
///
/// # Returns
///
/// `None`, queueing nothing, when no `zaura_shell` is bound or the bound
/// shell predates the child's creation request.
pub(crate) fn create(ctx: &mut Context, child: AuraChild, parent_host_id: u32) -> Option<u32> {
    create_from_parts(
        &mut ctx.shadow_table,
        &mut ctx.client_to_host_queue,
        ctx.host_zaura_shell_id,
        ctx.host_zaura_shell_version,
        child,
        parent_host_id,
    )
}

/// [`create`] on borrowed `Context` fields, for a caller that already
/// borrows another field (the output registry).
fn create_from_parts(
    shadow_table: &mut ShadowTable,
    client_to_host_queue: &mut Vec<(Vec<u8>, Vec<RawFd>)>,
    shell_host_id: Option<u32>,
    shell_version: u32,
    child: AuraChild,
    parent_host_id: u32,
) -> Option<u32> {
    let shell_host_id = shell_host_id?;
    let get = child.get_request();
    if shell_version < get.since {
        return None;
    }
    let child_host_id = shadow_table.allocate_host_id();
    shadow_table.track_host_interface_with_version(
        child_host_id,
        child.interface().to_string(),
        shell_version,
    );
    let mut builder = MessageBuilder::new();
    builder.write_u32(child_host_id);
    builder.write_u32(parent_host_id);
    let message = builder.build_message(shell_host_id, get.opcode);
    client_to_host_queue.push((message, Vec::new()));
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
    let release = child.release_request();
    if aura_object_version(ctx, child_host_id) >= release.since {
        let message = MessageBuilder::new().build_message(child_host_id, release.opcode);
        ctx.client_to_host_queue.push((message, Vec::new()));
        ctx.shadow_table.mark_pending_destroy_host(child_host_id);
    } else {
        ctx.shadow_table.retire_host_interface(child_host_id);
    }
}

/// Create or reuse the host Aura surface of a guest wl_surface.
///
/// XDG and GTK metadata and placement all target the same host surface, and
/// Exo rejects a second `zaura_surface` for one `wl_surface`, so every path
/// resolves it here.
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
    let zaura_surface_host_id = create(ctx, AuraChild::Surface, wl_surface_host_id)?;
    ctx.aura_surfaces
        .remember(wl_surface_host_id, zaura_surface_host_id);
    Some(zaura_surface_host_id)
}

/// Release the Aura surface of a host wl_surface that is being destroyed,
/// if it has one. Called before the wl_surface destructor is queued.
pub(crate) fn release_host_zaura_surface(ctx: &mut Context, wl_surface_host_id: u32) {
    if let Some(zaura_surface_host_id) = ctx.aura_surfaces.take(wl_surface_host_id) {
        release(ctx, AuraChild::Surface, zaura_surface_host_id);
    }
}

/// Return the host-only `zaura_output` of a bound output, creating it if
/// the output is still live (see [`crate::state::OutputRegistry::ensure_aura_output`]).
///
/// It carries the Aura scale factors, logical transform and work-area insets
/// that placement needs. The guest only sees the plain `wl_output`;
/// `wl_output.release` releases this child first.
///
/// # Returns
///
/// The `zaura_output` host ID, or `None` when the output has none and is not
/// live, or no `zaura_shell` that supports Aura outputs is bound yet.
pub(crate) fn ensure_zaura_output(ctx: &mut Context, wl_output_host_id: u32) -> Option<u32> {
    let shadow_table = &mut ctx.shadow_table;
    let client_to_host_queue = &mut ctx.client_to_host_queue;
    let (shell_host_id, shell_version) = (ctx.host_zaura_shell_id, ctx.host_zaura_shell_version);
    ctx.outputs.ensure_aura_output(wl_output_host_id, || {
        create_from_parts(
            shadow_table,
            client_to_host_queue,
            shell_host_id,
            shell_version,
            AuraChild::Output,
            wl_output_host_id,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_HOST: u32 = 0xff00_0001;
    const PARENT_HOST: u32 = 0xff00_0002;
    const ALL: [AuraChild; 3] = [AuraChild::Surface, AuraChild::Toplevel, AuraChild::Output];

    fn ctx_with_shell(version: u32) -> Context {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_zaura_shell_id = Some(SHELL_HOST);
        ctx.host_zaura_shell_version = version;
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
        for child in ALL {
            let version = child.get_request().since;
            let mut ctx = ctx_with_shell(version);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            assert_eq!(queued(&ctx), vec![(SHELL_HOST, child.get_request().opcode)]);
            assert_eq!(
                ctx.client_to_host_queue[0].0[8..16],
                [id.to_ne_bytes(), PARENT_HOST.to_ne_bytes()].concat()
            );
            assert_eq!(
                ctx.shadow_table.get_host_interface(id).map(String::as_str),
                Some(child.interface())
            );
            assert_eq!(aura_object_version(&ctx, id), version);
        }
    }

    #[test]
    fn create_needs_a_shell_that_has_the_request() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        assert_eq!(create(&mut ctx, AuraChild::Surface, PARENT_HOST), None);

        let mut ctx = ctx_with_shell(AuraChild::Output.get_request().since - 1);
        assert_eq!(create(&mut ctx, AuraChild::Output, PARENT_HOST), None);
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn release_follows_each_childs_release_version() {
        for child in ALL {
            let since = child.release_request().since;
            let mut ctx = ctx_with_shell(since);
            let id = create(&mut ctx, child, PARENT_HOST).expect("Aura child");
            ctx.client_to_host_queue.clear();
            release(&mut ctx, child, id);
            assert_eq!(queued(&ctx), vec![(id, child.release_request().opcode)]);
            assert!(ctx.shadow_table.is_pending_destroy_host_only(id));

            let mut ctx = ctx_with_shell(since - 1);
            let Some(id) = create(&mut ctx, child, PARENT_HOST) else {
                continue;
            };
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
}
