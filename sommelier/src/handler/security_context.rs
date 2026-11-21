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
use crate::protocols::wayland::wl_display;
use crate::protocols::Iface;
use crate::state::{Context, GuestId, RequestSender};
use crate::wire::Action;
use nix::sys::socket::{getsockname, getsockopt, sockopt, SockType, UnixAddr};
use nix::sys::stat::{fstat, SFlag};
use std::cell::Cell;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How many committed security contexts the proxy serves at once. Each
/// holds two descriptors and an accept loop until its engine hangs up,
/// also past the connection that made it, so a client that commits
/// without end would otherwise exhaust the process's descriptors; a
/// sandboxed app instance takes one.
pub(crate) const MAX_SANDBOX_LISTENERS: usize = 64;

/// What a committed security context hands the proxy: a socket to accept
/// sandboxed clients on until `close` signals, and the engine's metadata,
/// which only the log reads. It counts as live until it drops.
#[derive(Debug)]
pub(crate) struct SandboxListener {
    pub(crate) listen: OwnedFd,
    pub(crate) close: OwnedFd,
    pub(crate) metadata: Metadata,
    /// Its place in the count of live listeners, which it leaves as it
    /// drops: the accept loop keeps it until the engine hangs up.
    pub(crate) live: Live,
}

#[cfg(test)]
impl SandboxListener {
    /// A listener no security context committed, counted nowhere.
    pub(crate) fn for_test(listen: OwnedFd, close: OwnedFd) -> Self {
        let live = Rc::new(Cell::new(1));
        Self {
            listen,
            close,
            metadata: Metadata::default(),
            live: Live(live),
        }
    }
}

/// One live listener in [`SandboxListeners`]' count, until it drops.
#[derive(Debug)]
pub(crate) struct Live(Rc<Cell<usize>>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// A security context's metadata, each set at most once.
#[derive(Debug, Default)]
pub(crate) struct Metadata {
    pub(crate) engine: Option<String>,
    pub(crate) app_id: Option<String>,
    pub(crate) instance_id: Option<String>,
}

/// Where a connection hands committed security contexts: the proxy's
/// accept loop, while fewer than [`MAX_SANDBOX_LISTENERS`] are live.
#[derive(Clone, Debug)]
pub(crate) struct SandboxListeners {
    sender: UnboundedSender<SandboxListener>,
    live: Rc<Cell<usize>>,
}

impl SandboxListeners {
    /// Where connections hand committed security contexts, and where the
    /// accept loop receives them.
    pub(crate) fn new() -> (Self, UnboundedReceiver<SandboxListener>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let live = Rc::default();
        (Self { sender, live }, receiver)
    }

    /// Hand the accept loop a committed context; `false`, dropping it,
    /// while [`MAX_SANDBOX_LISTENERS`] are live.
    fn hand(&self, listen: OwnedFd, close: OwnedFd, metadata: Metadata) -> bool {
        if self.live.get() >= MAX_SANDBOX_LISTENERS {
            return false;
        }
        self.live.set(self.live.get() + 1);
        let listener = SandboxListener {
            listen,
            close,
            metadata,
            live: Live(Rc::clone(&self.live)),
        };
        // An accept loop that has ended drops it, which closes the socket.
        let _ = self.sender.send(listener);
        true
    }
}

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

/// Whether `fd` is a Unix stream socket that listens, which Wayland
/// clients can connect to.
fn listens(fd: BorrowedFd<'_>) -> bool {
    getsockopt(&fd, sockopt::AcceptConn).unwrap_or(false)
        && getsockopt(&fd, sockopt::SockType).is_ok_and(|kind| kind == SockType::Stream)
        && getsockname::<UnixAddr>(fd.as_raw_fd()).is_ok()
}

/// Whether `fd` can signal a hangup the event loop sees: a pipe, of
/// either end, or a socket.
fn can_hang_up(fd: BorrowedFd<'_>) -> bool {
    fstat(fd).is_ok_and(|stat| {
        let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
        kind == SFlag::S_IFIFO || kind == SFlag::S_IFSOCK
    })
}

impl wp_security_context_manager_v1::WpSecurityContextManagerV1Handler for Handlers {
    fn on_create_listener(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        listen_fd: BorrowedFd<'_>,
        close_fd: BorrowedFd<'_>,
    ) -> Action {
        if !listens(listen_fd) || !can_hang_up(close_fd) {
            return ctx.reject(
                sender.guest,
                wp_security_context_manager_v1::ERROR_INVALID_LISTEN_FD,
                "listen_fd is no listening Unix stream socket, or close_fd no pipe or socket",
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
        if listeners.hand(listen, close, metadata) {
            return Action::Drop;
        }
        log::warn!("Refusing a security context: {MAX_SANDBOX_LISTENERS} are served");
        ctx.reject(
            GuestId::DISPLAY,
            wl_display::ERROR_NO_MEMORY,
            format!("{MAX_SANDBOX_LISTENERS} security contexts are served"),
        )
    }
}

#[cfg(test)]
mod tests;
