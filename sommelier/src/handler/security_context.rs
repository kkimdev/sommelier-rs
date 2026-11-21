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

//! The guest's `wp_security_context_v1`: a sandbox engine, such as
//! Flatpak, hands the proxy a listening socket for its sandboxed clients,
//! and the proxy keeps from every client that connects through it the
//! globals that would widen its sandbox ([`Sandboxing`]).
//!
//! The manager and its contexts are local objects. A context collects the
//! engine's metadata until `commit` hands its socket to the proxy's accept
//! loop (`proxy::serve`) as a [`SandboxListener`]; the proxy then accepts
//! clients on it, each marked sandboxed (`ConnectionFacts::sandboxed`),
//! until the engine hangs up the context's close descriptor, also after
//! the engine's own connection ends, as the protocol asks.
//!
//! A sandboxed connection is offered neither this manager, so contexts
//! never nest, nor `ext_data_control_v1`: a Flatpak app reaches the
//! Wayland socket while its session bus is filtered, so data-control is
//! the one way it could read or replace the clipboard without the focus
//! (DESIGN "Data-control"). It still sets and reads the clipboard of its
//! own focused window, as any client does.

use crate::handler::Handlers;
use crate::protocols::security_context_v1::{
    wp_security_context_manager_v1, wp_security_context_v1,
};
use crate::protocols::Iface;
use crate::state::{Context, GuestId, RequestSender};
use crate::wire::Action;
use std::os::fd::{BorrowedFd, OwnedFd};
use tokio::sync::mpsc::UnboundedSender;

/// What a committed security context hands the proxy: a socket to accept
/// sandboxed clients on until `close` hangs up, and the engine's metadata,
/// which only the log reads.
#[derive(Debug)]
pub(crate) struct SandboxListener {
    pub(crate) listen: OwnedFd,
    pub(crate) close: OwnedFd,
    pub(crate) metadata: Metadata,
}

/// A security context's metadata, each set at most once.
#[derive(Debug, Default)]
pub(crate) struct Metadata {
    pub(crate) engine: Option<String>,
    pub(crate) app_id: Option<String>,
    pub(crate) instance_id: Option<String>,
}

/// Where a connection hands committed security contexts: the proxy's
/// accept loop.
pub(crate) type SandboxListeners = UnboundedSender<SandboxListener>;

/// One connection's sandboxing (`Context::sandboxing`), decided as it
/// connects.
#[derive(Debug, Default)]
pub(crate) struct Sandboxing {
    /// Where its security contexts go; `None` for a connection that is
    /// offered no manager: a sandboxed one, or a test's.
    listeners: Option<SandboxListeners>,
    /// Whether it came through a security context.
    sandboxed: bool,
}

impl Sandboxing {
    /// A connection's sandboxing: `sandboxed` if it came through a
    /// security context; otherwise offered the manager when `listeners`
    /// is given.
    pub(crate) fn new(listeners: Option<SandboxListeners>, sandboxed: bool) -> Self {
        Self {
            listeners: listeners.filter(|_| !sandboxed),
            sandboxed,
        }
    }

    /// Whether the guest is offered `wp_security_context_manager_v1`.
    pub(crate) fn offers_manager(&self) -> bool {
        self.listeners.is_some()
    }

    /// Whether the connection came through a security context.
    pub(crate) fn is_sandboxed(&self) -> bool {
        self.sandboxed
    }
}

/// A guest `wp_security_context_v1`'s state.
#[derive(Debug, Default)]
pub(crate) struct SecurityContext {
    /// Its listening socket and close descriptor, until `commit` hands
    /// them on.
    descriptors: Option<(OwnedFd, OwnedFd)>,
    metadata: Metadata,
    committed: bool,
}

/// Whether `fd` is a socket that listens: `SO_ACCEPTCONN`.
fn listens(fd: BorrowedFd<'_>) -> bool {
    nix::sys::socket::getsockopt(&fd, nix::sys::socket::sockopt::AcceptConn).unwrap_or(false)
}

impl wp_security_context_manager_v1::WpSecurityContextManagerV1Handler for Handlers {
    fn on_create_listener(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        listen_fd: BorrowedFd<'_>,
        close_fd: BorrowedFd<'_>,
    ) -> Action {
        if !listens(listen_fd) {
            return ctx.reject(
                sender.guest,
                wp_security_context_manager_v1::ERROR_INVALID_LISTEN_FD,
                "listen_fd is not a listening socket",
            );
        }
        // The request closes its descriptors once handled.
        let descriptors = match (
            listen_fd.try_clone_to_owned(),
            close_fd.try_clone_to_owned(),
        ) {
            (Ok(listen), Ok(close)) => (listen, close),
            (Err(error), _) | (_, Err(error)) => {
                return ctx.fail(format_args!(
                    "Cannot keep a security context's socket: {error}"
                ));
            }
        };
        let context = SecurityContext {
            descriptors: Some(descriptors),
            ..SecurityContext::default()
        };
        ctx.shadow_table
            .create_local_with(id, Iface::WpSecurityContextV1, sender.version, context);
        Action::Drop
    }
}

/// Set metadata `field` of security context `sender` to `value`, once and
/// before its commit.
fn set_metadata(
    ctx: &mut Context,
    sender: RequestSender,
    value: &str,
    field: fn(&mut Metadata) -> &mut Option<String>,
) -> Action {
    let Some(context) = ctx.shadow_table.data_mut::<SecurityContext>(sender.guest) else {
        return ctx.fail(format_args!(
            "Security context {} has no state",
            sender.guest
        ));
    };
    let error = if context.committed {
        wp_security_context_v1::ERROR_ALREADY_USED
    } else if field(&mut context.metadata).is_some() {
        wp_security_context_v1::ERROR_ALREADY_SET
    } else {
        *field(&mut context.metadata) = Some(value.to_string());
        return Action::Drop;
    };
    ctx.reject(
        sender.guest,
        error,
        "the security context's metadata is fixed",
    )
}

impl wp_security_context_v1::WpSecurityContextV1Handler for Handlers {
    fn on_set_sandbox_engine(ctx: &mut Context, sender: RequestSender, name: &String) -> Action {
        set_metadata(ctx, sender, name, |metadata| &mut metadata.engine)
    }

    fn on_set_app_id(ctx: &mut Context, sender: RequestSender, app_id: &String) -> Action {
        set_metadata(ctx, sender, app_id, |metadata| &mut metadata.app_id)
    }

    fn on_set_instance_id(
        ctx: &mut Context,
        sender: RequestSender,
        instance_id: &String,
    ) -> Action {
        set_metadata(ctx, sender, instance_id, |metadata| {
            &mut metadata.instance_id
        })
    }

    fn on_commit(ctx: &mut Context, sender: RequestSender) -> Action {
        let Some(context) = ctx.shadow_table.data_mut::<SecurityContext>(sender.guest) else {
            return ctx.fail(format_args!(
                "Security context {} has no state",
                sender.guest
            ));
        };
        if std::mem::replace(&mut context.committed, true) {
            return ctx.reject(
                sender.guest,
                wp_security_context_v1::ERROR_ALREADY_USED,
                "the security context was committed",
            );
        }
        let (Some((listen, close)), Some(listeners)) =
            (context.descriptors.take(), &ctx.sandboxing.listeners)
        else {
            return Action::Drop;
        };
        let metadata = std::mem::take(&mut context.metadata);
        // An accept loop that has ended drops it, which closes the socket.
        let _ = listeners.send(SandboxListener {
            listen,
            close,
            metadata,
        });
        Action::Drop
    }
}

#[cfg(test)]
mod tests;
