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

use crate::handler::callback::queue_internal_sync;
use crate::handler::linux_dmabuf::GUEST_DMABUF_VERSION;
use crate::handler::shm::{
    clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, register_guest_shm,
};
use crate::handler::Handlers;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
use crate::protocols::wayland::{wl_display, wl_fixes, wl_registry, wl_surface};
use crate::protocols::Iface;
use crate::settings::Settings;
use crate::state::{
    Binding, Context, EventSender, GuestId, HostId, InternalCallback, RequestObject, RequestSender,
    Singleton, INERT_BIND_INTERFACE,
};
use crate::state::{RegistryRecord, Singletons};
use crate::wire::Action;
use log::error;
use std::collections::{BTreeMap, BTreeSet};

/// The newest `zwp_linux_dmabuf_v1` the proxy binds on the host. This is a
/// different fact from the guest version: the legacy format and modifier
/// events, which are deprecated from the feedback version on, are what the
/// proxy synthesizes the guest's feedback object from. `create_immed` is
/// available since v2 and explicit modifier events since v3, so bind at v3
/// when the host supports it.
pub(crate) const LINUX_DMABUF_CAPABILITY_VERSION: u32 = zwp_linux_dmabuf_v1::EVT_MODIFIER_SINCE;
// The host binding has no feedback requests, so no host feedback object
// exists (see `ZwpLinuxDmabufFeedbackV1Handler for LinuxDmabufHandler`).
const _: () =
    assert!(LINUX_DMABUF_CAPABILITY_VERSION < zwp_linux_dmabuf_v1::REQ_GET_DEFAULT_FEEDBACK_SINCE);
/// The `wl_compositor` ceiling, at which a host of
/// [`WL_COMPOSITOR_HOST_VERSION`] or newer is offered, as ChromiumOS
/// Sommelier offers it with `--support-damage-buffer`: v4 adds only
/// `wl_surface.damage_buffer`, which the proxy records and sends the host
/// as `wl_surface.damage` at commit. The proxy does not implement the v5
/// and v6 surface semantics: it reads attach's offset, which v5 replaced
/// with `wl_surface.offset`, and the generated version check rejects the
/// latter, so raising the ceiling needs that handled first.
const WL_COMPOSITOR_VERSION: u32 = 4;
const _: () = assert!(WL_COMPOSITOR_VERSION < wl_surface::REQ_OFFSET_SINCE);
/// The newest `wl_compositor` the proxy binds for the guest: v3 provides
/// buffer scale and transform, and the proxy implements v4's
/// `damage_buffer` itself.
const WL_COMPOSITOR_HOST_VERSION: u32 = 3;
/// The `xdg_wm_base` ceiling, ChromiumOS Sommelier's
/// (`SL_XDG_SHELL_MAX_VERSION` in `sommelier-xdg-shell.h`). The vendored
/// XML is newer: v4 to v7 add configure bounds, window manager
/// capabilities, and the suspended and constrained states, which the proxy
/// has not been checked against.
const XDG_SHELL_VERSION: u32 = 3;
/// The `wl_shm` version the proxy offers the guest and binds on the host.
/// The proxy implements the guest's `wl_shm` itself, as a v1 compatibility
/// shim (`handler/shm/`), so a newer host or XML version is not offered.
const WL_SHM_VERSION: u32 = 1;
/// The `wl_output` ceiling; [`offered_version`] says why. The
/// vendored XML is newer: v4 adds the `name` and `description` events.
const WL_OUTPUT_VERSION: u32 = 3;
/// The `wl_seat` ceiling; [`offered_version`] says why. The
/// vendored XML is newer: v6 to v10 add touch shape and orientation,
/// high-resolution scroll and scroll-direction events, and, at v10, the
/// `repeated` key state.
pub(crate) const WL_SEAT_VERSION: u32 = 5;
/// The oldest `zaura_shell` this proxy binds, the v6+ contract ChromiumOS
/// Sommelier uses. Every Aura child takes its shell's version, so this
/// floor is what lets the proxy create Aura surfaces and outputs, and send
/// `set_application_id` and `set_startup_id`, without a per-object version
/// check. The assertions keep that true if the vendored protocol changes.
const ZAURA_SHELL_MIN_VERSION: u32 = 6;
const _: () = assert!(
    crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE_SINCE
        <= ZAURA_SHELL_MIN_VERSION
);
const _: () = assert!(
    crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_OUTPUT_SINCE <= ZAURA_SHELL_MIN_VERSION
);
const _: () = assert!(
    crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID_SINCE
        <= ZAURA_SHELL_MIN_VERSION
);
const _: () = assert!(
    crate::protocols::aura_shell::zaura_surface::REQ_SET_STARTUP_ID_SINCE
        <= ZAURA_SHELL_MIN_VERSION
);

/// What the proxy does with a global of an interface: its row in
/// [`policy`].
#[derive(Clone, Copy, Debug)]
enum Policy {
    /// Forward a host global of the interface to the guest while `enabled`
    /// says so, offered at the host's version capped at `ceiling`
    /// ([`offered_version`]), and hide it otherwise. The guest's bind is
    /// served as `serve` says.
    Forward {
        ceiling: u32,
        enabled: fn(&Settings) -> bool,
        serve: Serve,
    },
    /// Bind a host global of the interface for the proxy's own use, and
    /// offer the guest what it carries in its place ([`Internal`]).
    Bind(Internal),
    /// Hide a host global of the interface: the guest binds the proxy's
    /// own global of it instead, which another interface's row carries
    /// ([`Carries`]), and its bind is served as this says.
    Synthetic(Serve),
    /// Hide a host global of the interface, and offer the guest none. An
    /// interface that is no global, which no compositor advertises, is
    /// hidden too.
    Hide,
}

/// How the proxy binds a host global for its own use, and what the guest
/// is offered under the global's name instead.
#[derive(Clone, Copy, Debug)]
struct Internal {
    /// The binding: one per connection ([`Globals::singletons`]).
    singleton: Singleton,
    /// Whether the proxy uses a global the host advertises at the given
    /// version; one it does not use is hidden, and never bound.
    binds: fn(&Context, u32) -> bool,
    /// The newest version the proxy binds: the vendored XML's, the newest
    /// the generated code can decode, unless a named constant says why not.
    ceiling: u32,
    /// What the guest is offered under the global's name.
    carries: Carries,
    /// How the guest's bind is served, for a global that carries its own
    /// interface.
    serve: Option<Serve>,
}

/// A global of the proxy's own that the guest is offered under the name of
/// a host global the proxy binds ([`Internal`]), so the host's
/// `global_remove` of it removes the guest's too. Each is carried by one
/// row (see [`check_table`]).
#[derive(Clone, Copy, Debug)]
enum Carries {
    /// A global of `iface` at `version`, while `offered` says so; the host
    /// global is hidden otherwise.
    Global {
        iface: Iface,
        version: u32,
        offered: fn(&Context) -> bool,
    },
    /// The guest's `zwp_linux_dmabuf_v1`, which may wait for the
    /// capabilities of the proxy's binding ([`offer_dmabuf_global`]).
    Dmabuf,
}

impl Carries {
    /// The interface of the global the guest is offered.
    const fn iface(self) -> Iface {
        match self {
            Self::Global { iface, .. } => iface,
            Self::Dmabuf => Iface::ZwpLinuxDmabufV1,
        }
    }
}

/// How the proxy serves a guest's bind of a global it advertised.
#[derive(Clone, Copy, Debug)]
enum Serve {
    /// Bind the host global for the guest, at the version the guest asks
    /// for, but at most at `host_ceiling`: the proxy implements the
    /// versions above it itself. Then do what `then` says.
    Paired {
        host_ceiling: u32,
        then: Option<AfterBind>,
    },
    /// A local object: the proxy implements the global itself, and nothing
    /// reaches the host.
    Local,
    /// The proxy's own `wl_shm` (`handler::shm`).
    Shm,
    /// The proxy's own `gtk_shell1` (`handler::gtk_shell`).
    GtkShell,
}

/// What follows a paired bind ([`Serve::Paired`]), once it is queued.
#[derive(Clone, Copy, Debug)]
enum AfterBind {
    /// Record the output, and create placement's Aura output for it.
    Output,
    /// Let a text input whose seat had no host object activate.
    Seat,
    /// Hold the global's capabilities for the guest's factory.
    DmabufFactory,
}

/// What the connection knows of the host's globals and the proxy's own
/// bindings of them (`Context::globals`). Its fields are this module's, so
/// only the registry policy adds, retires or binds a global; other modules
/// read the bindings ([`Self::singletons`]).
#[derive(Default)]
pub(crate) struct Globals {
    /// Host globals visible to the guest, keyed by their unique numeric name.
    host_globals: BTreeMap<u32, HostGlobal>,
    /// Every host global the proxy may bind as a singleton, by name, with
    /// the singleton and the host's version, while a registry lists it;
    /// see [`Singleton`].
    singleton_candidates: BTreeMap<u32, (Singleton, u32)>,
    /// What each host registry has advertised, by host registry ID (see
    /// [`RegistryRecord`]).
    registries: BTreeMap<HostId, RegistryRecord>,
    /// Every global name whose removal a registry has seen. A name is one
    /// global for the connection's life, so it stays here: a global event
    /// that names it again breaks the protocol ([`accept_global`]). It
    /// outlives the registries that saw the removal, which a guest may
    /// destroy before the last registry that lists the global sees it. It
    /// grows by one name per global removal, for the connection's life:
    /// negligible, and by design, because libwayland never reuses a name.
    removed_globals: BTreeSet<u32>,
    /// The proxy's binding of each host global it binds once per
    /// connection, for its own use.
    singletons: Singletons,
    /// Guest-facing v4 linux-dmabuf globals withheld until the internal v3
    /// binding has delivered its complete legacy format/modifier
    /// capability set.
    pending_dmabuf: Vec<PendingDmabufGlobal>,
}

/// One global advertised by the host compositor, by its name in
/// [`Globals`]: names are unique, but a compositor may advertise several
/// globals of one interface, such as `wl_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostGlobal {
    /// The interface the guest knows the global by.
    interface: Iface,
    /// The newest version the guest may bind.
    version: u32,
}

/// A synthetic linux-dmabuf v4 global waiting for legacy host capability
/// discovery to complete.
///
/// The host registry object is retained because each guest registry needs its
/// own `global` event; the registry's removal of the global drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDmabufGlobal {
    registry_host_id: HostId,
    name: u32,
    version: u32,
}

impl Globals {
    /// The proxy's binding of each singleton.
    pub(crate) fn singletons(&self) -> &Singletons {
        &self.singletons
    }

    /// Whether a guest global waits for the capabilities of host
    /// linux-dmabuf global `global`.
    pub(crate) fn awaits_dmabuf_capabilities(&self, global: u32) -> bool {
        self.pending_dmabuf
            .iter()
            .any(|pending| pending.name == global)
    }

    /// Whether a registry still lists global `name`: it has not seen its
    /// `global_remove` yet.
    pub(crate) fn still_seen(&self, name: u32) -> bool {
        self.listing(name).is_some()
    }

    /// The lowest host registry that still lists global `name`, if any (see
    /// [`Self::still_seen`]).
    fn listing(&self, name: u32) -> Option<HostId> {
        self.registries
            .iter()
            .filter(|(_, record)| record.lists(name))
            .map(|(&registry, _)| registry)
            .min()
    }

    /// Bind `singleton` as host object `host`, as the registry would.
    #[cfg(test)]
    pub(crate) fn bind_for_test(&mut self, singleton: Singleton, host: HostId) {
        self.singletons.bind_for_test(singleton, host);
    }

    /// Forget `singleton`'s binding, for a test of a proxy that has none.
    #[cfg(test)]
    pub(crate) fn unbind_for_test(&mut self, singleton: Singleton) {
        self.singletons.unbind_for_test(singleton);
    }

    /// Panic if a registry record outlived its registry, or a global or
    /// candidate outlived every registry that lists it.
    #[cfg(test)]
    pub(crate) fn assert_no_orphans(&self, table: &crate::state::ShadowTable) {
        let pending = self
            .pending_dmabuf
            .iter()
            .map(|pending| &pending.registry_host_id);
        for &registry in self.registries.keys().chain(pending) {
            assert!(
                table
                    .get_guest_id(registry)
                    .is_some_and(|guest| table.live_object(guest, Iface::WlRegistry).is_some()),
                "registry globals outlived host wl_registry {registry}"
            );
        }
        // A singleton candidate is a name a registry still lists, and so
        // is an advertised global whose removal a registry has seen: it
        // retires with the last registry that lists it.
        for &name in self.singleton_candidates.keys() {
            assert!(
                self.still_seen(name),
                "singleton candidate {name} outlived every registry that lists it"
            );
        }
        for &name in self.host_globals.keys() {
            assert!(
                !self.removed_globals.contains(&name) || self.still_seen(name),
                "removed global {name} outlived every registry that lists it"
            );
        }
    }

    /// The names of the singleton candidates, for a test.
    #[cfg(test)]
    pub(crate) fn singleton_candidates_for_test(&self) -> Vec<u32> {
        self.singleton_candidates.keys().copied().collect()
    }
}

/// A global forwarded whatever the settings, offered at most at `ceiling`,
/// whose guest bind is paired with a host binding of the same version.
const fn forward(ceiling: u32) -> Policy {
    forward_then(ceiling, None)
}

/// [`forward`], doing what `then` says after the guest's bind.
const fn forward_then(ceiling: u32, then: Option<AfterBind>) -> Policy {
    Policy::Forward {
        ceiling,
        enabled: |_| true,
        serve: Serve::Paired {
            host_ceiling: ceiling,
            then,
        },
    }
}

/// What the proxy does with a global of `iface`: the one table of the
/// globals it knows. Its `match` names every interface, so a new interface
/// needs a row, and a forwarded global needs nothing more.
///
/// A version bound is the generated `VERSION` unless a named constant above
/// documents why it deliberately differs; [`check_table`] holds the rows to
/// the rules the `match` cannot.
const fn policy(iface: Iface) -> Policy {
    match iface {
        Iface::WlCompositor => Policy::Forward {
            ceiling: WL_COMPOSITOR_VERSION,
            enabled: |_| true,
            serve: Serve::Paired {
                host_ceiling: WL_COMPOSITOR_HOST_VERSION,
                then: None,
            },
        },
        Iface::WlSubcompositor => forward(Iface::WlSubcompositor.version()),
        Iface::WlDataDeviceManager => forward(Iface::WlDataDeviceManager.version()),
        Iface::WlShell => forward(Iface::WlShell.version()),
        Iface::WlSeat => forward_then(WL_SEAT_VERSION, Some(AfterBind::Seat)),
        Iface::WlOutput => forward_then(WL_OUTPUT_VERSION, Some(AfterBind::Output)),
        Iface::WlFixes => forward(Iface::WlFixes.version()),
        Iface::XdgWmBase => forward(XDG_SHELL_VERSION),
        Iface::WpViewporter => forward(Iface::WpViewporter.version()),
        Iface::WpFractionalScaleManagerV1 => forward(Iface::WpFractionalScaleManagerV1.version()),
        Iface::ZxdgOutputManagerV1 => forward(Iface::ZxdgOutputManagerV1.version()),
        // A host's own v3 manager is offered too. Every guest manager is
        // local: they share the proxy's one host v1 manager, whose ID no
        // guest manager may own, or one's destroy would end it for all.
        Iface::ZwpTextInputManagerV3 => Policy::Forward {
            ceiling: Iface::ZwpTextInputManagerV3.version(),
            enabled: |_| true,
            serve: Serve::Local,
        },
        Iface::ZxdgDecorationManagerV1 => Policy::Forward {
            ceiling: Iface::ZxdgDecorationManagerV1.version(),
            enabled: |settings| settings.xdg_decoration,
            serve: Serve::Paired {
                host_ceiling: Iface::ZxdgDecorationManagerV1.version(),
                then: None,
            },
        },
        // The proxy emulates the guest's wl_shm, so that it offers only the
        // formats the host can import; its binding receives the host's
        // formats.
        Iface::WlShm => Policy::Bind(Internal {
            singleton: Singleton::Shm,
            binds: |_, _| true,
            ceiling: WL_SHM_VERSION,
            carries: Carries::Global {
                iface: Iface::WlShm,
                version: WL_SHM_VERSION,
                offered: |_| true,
            },
            serve: Some(Serve::Shm),
        }),
        // The binding learns the host's formats and modifiers, which the
        // guest's feedback and wl_shm are made from.
        Iface::ZwpLinuxDmabufV1 => Policy::Bind(Internal {
            singleton: Singleton::Dmabuf,
            binds: |ctx, _| dmabuf_binds(ctx),
            ceiling: LINUX_DMABUF_CAPABILITY_VERSION,
            carries: Carries::Dmabuf,
            // The guest's feedback requests are the proxy's own.
            serve: Some(Serve::Paired {
                host_ceiling: LINUX_DMABUF_CAPABILITY_VERSION,
                then: Some(AfterBind::DmabufFactory),
            }),
        }),
        // The ack-key mechanism for the host's accelerators, and, from v2,
        // the keys the IME consumed (`peek_key`).
        Iface::ZcrKeyboardExtensionV1 => Policy::Bind(Internal {
            singleton: Singleton::KeyboardExtension,
            binds: |_, _| true,
            ceiling: Iface::ZcrKeyboardExtensionV1.version(),
            // With data-control on. The manager needs no host global of
            // its own (`handler::data_control`), and every Exo has this
            // one, which the guest never sees, so its removal stays paired.
            carries: Carries::Global {
                iface: Iface::ExtDataControlManagerV1,
                version: Iface::ExtDataControlManagerV1.version(),
                offered: |ctx| ctx.data_control.offers_global(),
            },
            serve: None,
        }),
        // For ChromeOS shelf integration and window placement: the proxy
        // creates Aura surfaces from it to set application and startup IDs.
        Iface::ZauraShell => Policy::Bind(Internal {
            singleton: Singleton::AuraShell,
            binds: |_, version| version >= ZAURA_SHELL_MIN_VERSION,
            ceiling: Iface::ZauraShell.version(),
            carries: Carries::Global {
                iface: Iface::GtkShell1,
                version: Iface::GtkShell1.version(),
                offered: |_| true,
            },
            serve: None,
        }),
        // Text inputs are created from the binding (`handler::text_input`).
        Iface::ZwpTextInputManagerV1 => Policy::Bind(Internal {
            singleton: Singleton::TextInputManager,
            binds: |_, _| true,
            ceiling: Iface::ZwpTextInputManagerV1.version(),
            carries: Carries::Global {
                iface: Iface::ZwpTextInputManagerV3,
                version: Iface::ZwpTextInputManagerV3.version(),
                offered: |_| true,
            },
            serve: None,
        }),
        Iface::ZcrTextInputExtensionV1 => Policy::Bind(Internal {
            singleton: Singleton::TextInputExtension,
            binds: |_, _| true,
            ceiling: Iface::ZcrTextInputExtensionV1.version(),
            // Unless the connection is offered none
            // (`handler::security_context`). The manager needs no host
            // global of its own, and every Exo has this one, which the
            // guest never sees.
            carries: Carries::Global {
                iface: Iface::WpSecurityContextManagerV1,
                version: Iface::WpSecurityContextManagerV1.version(),
                offered: |ctx| ctx.sandboxing.offers_manager(),
            },
            serve: None,
        }),
        Iface::GtkShell1 => Policy::Synthetic(Serve::GtkShell),
        // The proxy implements data-control and security contexts itself.
        Iface::ExtDataControlManagerV1 | Iface::WpSecurityContextManagerV1 => {
            Policy::Synthetic(Serve::Local)
        }
        // No globals.
        Iface::WlDisplay
        | Iface::WlRegistry
        | Iface::WlCallback
        | Iface::WlShmPool
        | Iface::WlBuffer
        | Iface::WlDataOffer
        | Iface::WlDataSource
        | Iface::WlDataDevice
        | Iface::WlShellSurface
        | Iface::WlSurface
        | Iface::WlPointer
        | Iface::WlKeyboard
        | Iface::WlTouch
        | Iface::WlRegion
        | Iface::WlSubsurface
        | Iface::XdgPositioner
        | Iface::XdgSurface
        | Iface::XdgToplevel
        | Iface::XdgPopup
        | Iface::ZwpLinuxBufferParamsV1
        | Iface::ZwpLinuxDmabufFeedbackV1
        | Iface::WpViewport
        | Iface::ZwpTextInputV3
        | Iface::ZwpTextInputV1
        | Iface::ZcrExtendedTextInputV1
        | Iface::ZxdgToplevelDecorationV1
        | Iface::WpFractionalScaleV1
        | Iface::ZxdgOutputV1
        | Iface::ZcrExtendedKeyboardV1
        | Iface::GtkSurface1
        | Iface::ZauraSurface
        | Iface::ZauraOutput
        | Iface::ZauraToplevel
        | Iface::ZauraPopup
        | Iface::ExtDataControlDeviceV1
        | Iface::ExtDataControlSourceV1
        | Iface::ExtDataControlOfferV1
        | Iface::WpSecurityContextV1 => Policy::Hide,
    }
}

impl Policy {
    /// How the proxy serves the guest's bind of a global of the interface,
    /// or `None` if it never offers the guest one.
    const fn serve(self) -> Option<Serve> {
        match self {
            Self::Forward { serve, .. } | Self::Synthetic(serve) => Some(serve),
            Self::Bind(internal) => internal.serve,
            Self::Hide => None,
        }
    }
}

/// The rules of [`policy`] its `match` cannot hold, checked as the crate
/// builds: no version above the XML, one row for each singleton, and one
/// row carrying each global the proxy offers in a host global's place,
/// whose bind it serves.
const fn check_table() {
    let mut i = 0;
    while i < Iface::ALL.len() {
        let iface = Iface::ALL[i];
        match policy(iface) {
            Policy::Forward { ceiling, serve, .. } => {
                assert!(
                    ceiling <= iface.version(),
                    "a global is offered above its XML"
                );
                if let Serve::Paired { host_ceiling, .. } = serve {
                    assert!(host_ceiling <= ceiling, "a global is bound above its offer");
                }
            }
            Policy::Bind(internal) => {
                assert!(
                    internal.ceiling <= iface.version(),
                    "a global is bound above its XML"
                );
                let carried = internal.carries.iface();
                assert!(carriers(carried) == 1, "two rows carry one global");
                assert!(
                    policy(carried).serve().is_some(),
                    "a carried global is never served"
                );
            }
            Policy::Synthetic(serve) => {
                assert!(carriers(iface) == 1, "no row carries a synthetic global");
                assert!(
                    !matches!(serve, Serve::Paired { .. }),
                    "a synthetic global has no host global to pair"
                );
            }
            Policy::Hide => {}
        }
        i += 1;
    }
    let mut i = 0;
    while i < SINGLETONS.len() {
        let singleton = SINGLETONS[i];
        // Names every variant, so a new one does not build until it is
        // listed in `SINGLETONS`, and so checked here.
        match singleton {
            Singleton::Dmabuf
            | Singleton::KeyboardExtension
            | Singleton::AuraShell
            | Singleton::Shm
            | Singleton::TextInputManager
            | Singleton::TextInputExtension => {}
        }
        assert!(binders(singleton) == 1, "a singleton has no row, or two");
        i += 1;
    }
}
const _: () = check_table();

/// Every singleton, which [`check_table`] holds to one row each.
const SINGLETONS: [Singleton; 6] = [
    Singleton::Dmabuf,
    Singleton::KeyboardExtension,
    Singleton::AuraShell,
    Singleton::Shm,
    Singleton::TextInputManager,
    Singleton::TextInputExtension,
];

/// How many rows of [`policy`] bind `singleton`.
const fn binders(singleton: Singleton) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < Iface::ALL.len() {
        if let Policy::Bind(internal) = policy(Iface::ALL[i]) {
            if internal.singleton as usize == singleton as usize {
                count += 1;
            }
        }
        i += 1;
    }
    count
}

/// How many rows of [`policy`] carry a global of `iface`.
const fn carriers(iface: Iface) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < Iface::ALL.len() {
        if let Policy::Bind(internal) = policy(Iface::ALL[i]) {
            if internal.carries.iface() as usize == iface as usize {
                count += 1;
            }
        }
        i += 1;
    }
    count
}

/// The version the guest is offered of a forwarded host global, capped at
/// `ceiling`, that the host advertises at `host_version`.
///
/// Some interfaces have a lower ceiling than the vendored XML, as in
/// ChromiumOS Sommelier's client-facing globals. The generated code can
/// decode newer XML messages, but that does not mean the proxy implements
/// the semantics of every newer version. In particular, the C++ proxy caps
/// `wl_seat` at v5 and `wl_output` at v3; it also exposes the compositor
/// only up to the version for which its surface damage handling is
/// implemented. Keep the guest advertisement aligned with those ceilings so
/// clients do not start using requests/events that this proxy merely
/// forwards without the required coordinate/focus handling.
fn offered_version(ceiling: u32, serve: Serve, host_version: u32) -> u32 {
    match serve {
        // A host that has every version the proxy binds for the guest is
        // offered the ceiling, since the proxy implements the versions
        // above: ChromiumOS implements wl_surface.damage_buffer in terms of
        // the legacy damage request, so a v3 host compositor is offered as
        // v4.
        Serve::Paired { host_ceiling, .. } if host_version >= host_ceiling => ceiling,
        Serve::Paired { .. } | Serve::Local | Serve::Shm | Serve::GtkShell => {
            host_version.min(ceiling)
        }
    }
}

/// Whether the proxy binds a host linux-dmabuf: with GPU access, for the
/// guest's, or with a VirtWL channel that imports dma-bufs, to learn the
/// formats the guest's `wl_shm` may offer.
fn dmabuf_binds(ctx: &Context) -> bool {
    let virtwl_supports_dmabuf = ctx
        .virtwayland_channel()
        .is_some_and(|channel| channel.supports_dmabuf());
    should_bind_internal_dmabuf(ctx.settings().gpu_accel, virtwl_supports_dmabuf)
}

fn should_bind_internal_dmabuf(gpu_accel: bool, virtwl_supports_dmabuf: bool) -> bool {
    gpu_accel || virtwl_supports_dmabuf
}

/// Whether the host removed global `name`, as a registry has seen.
fn global_removal_seen(ctx: &Context, name: u32) -> bool {
    ctx.globals.removed_globals.contains(&name)
}

/// Retire global `name`, which the host removed, once no registry still
/// lists it ([`Globals::still_seen`]): reset the internal binding it backed,
/// and drop its connection-wide metadata, whose absence refuses new binds.
///
/// `wl_registry.global_remove` invalidates only the *global name*. Any
/// object that was already bound remains valid until its own destructor
/// (the Wayland core protocol explicitly requires this), so an internal
/// object keeps its ID and singleton state reserved, and queued host events
/// cannot be routed to a later object that happens to reuse the ID.
fn retire_unseen_global(ctx: &mut Context, name: u32) {
    reset_internal_binding_for_global(ctx, name);
    crate::handler::shm::retire_guest_shms(ctx, name);
    ctx.globals.singleton_candidates.remove(&name);
    ctx.globals.host_globals.remove(&name);
    if ensure_all_bound(ctx).is_err() {
        // The failed bind ended the session in `ctx`, which is all its
        // result says; the message's handler cannot do more.
        debug_assert!(ctx.is_fatal());
    }
}

fn queue_synthetic_global_for_registry(
    ctx: &mut Context,
    registry_host_id: HostId,
    name: u32,
    interface: Iface,
    version: u32,
) -> bool {
    // A host registry is paired with its guest registry until the guest
    // destroys it, which drops what waits to be advertised on it.
    let Some(registry_guest_id) = ctx.shadow_table.get_guest_id(registry_host_id) else {
        debug_assert!(
            false,
            "host registry {registry_host_id} has no guest registry"
        );
        return false;
    };
    ctx.send_guest_or_drop(
        wl_registry::evt::global(registry_guest_id, name, interface.name(), version),
        format_args!("synthetic global {name}:{interface}"),
    )
}

fn defer_synthetic_dmabuf_global(
    ctx: &mut Context,
    registry_host_id: HostId,
    name: u32,
    version: u32,
) {
    let pending = PendingDmabufGlobal {
        registry_host_id,
        name,
        version,
    };
    if !ctx.globals.pending_dmabuf.contains(&pending) {
        ctx.globals.pending_dmabuf.push(pending);
    }
    set_registry_global_visibility(ctx, registry_host_id, name, false);
}

/// Whether the proxy may withhold a guest linux-dmabuf global until the
/// capabilities of its binding arrive ([`offer_dmabuf_global`]): with GPU
/// access, when it synthesizes the global's feedback.
fn dmabuf_globals_may_wait(ctx: &Context) -> bool {
    ctx.settings().gpu_accel
        && crate::handler::linux_dmabuf::can_synthesize_feedback(
            ctx,
            LINUX_DMABUF_CAPABILITY_VERSION,
        )
}

/// The guest created a registry, whose `get_registry` the proxy has just
/// sent the host. If the proxy may withhold one of its globals, it follows
/// the request with a sync ([`InternalCallback::RegistryGlobals`]), whose
/// `done` says that the host has sent the registry every global it had;
/// until then the guest's globals are not settled ([`globals_settled`]).
pub(crate) fn await_globals(ctx: &mut Context) {
    if dmabuf_globals_may_wait(ctx) {
        queue_internal_sync(ctx, InternalCallback::RegistryGlobals);
    }
}

/// Whether a guest `wl_display.sync` may reach the host: the host has
/// listed the globals of every registry the guest created before it
/// ([`await_globals`]). The sync waits until then, so that its `done`
/// follows the guest's globals, as libwayland's follows those of every
/// registry created before the sync, although the proxy withholds a
/// linux-dmabuf global until the capabilities of its binding arrive. The
/// proxy binds a global, and asks for its capabilities, when the host
/// lists it, so the sync reaches the host after those requests, the host
/// answers them first (E11), and the answer shows or hides the global
/// before the sync's `done` arrives. Only the proxy's own syncs keep the
/// globals unsettled, and the host answers each, so the wait ends.
pub(crate) fn globals_settled(ctx: &Context) -> bool {
    !ctx.internal_callbacks.awaits_registry_globals()
}

/// Publish the guest globals that wait for the capabilities of host
/// linux-dmabuf global `name`, which have arrived.
pub(crate) fn publish_pending_dmabuf_globals(ctx: &mut Context, name: u32) {
    let can_publish = crate::handler::linux_dmabuf::has_synthetic_feedback_formats(ctx, name);
    let mut matching = Vec::new();
    ctx.globals.pending_dmabuf.retain(|pending| {
        if pending.name == name {
            matching.push(pending.clone());
            false
        } else {
            true
        }
    });

    if !can_publish {
        log::warn!("Hiding linux-dmabuf {name} because the host advertised no supported formats");
        return;
    }

    for pending in matching {
        let listed = ctx
            .globals
            .registries
            .get(&pending.registry_host_id)
            .is_some_and(|record| record.lists(pending.name));
        if !listed {
            continue;
        }

        ctx.globals.host_globals.insert(
            pending.name,
            HostGlobal {
                interface: Iface::ZwpLinuxDmabufV1,
                version: pending.version,
            },
        );
        let visible = queue_synthetic_global_for_registry(
            ctx,
            pending.registry_host_id,
            pending.name,
            Iface::ZwpLinuxDmabufV1,
            pending.version,
        );
        set_registry_global_visibility(ctx, pending.registry_host_id, pending.name, visible);
        if !visible {
            ctx.globals.host_globals.remove(&pending.name);
        }
    }
}

fn queue_internal_bind(
    ctx: &mut Context,
    registry_host_id: HostId,
    name: u32,
    interface: Iface,
    version: u32,
    host_id: HostId,
) -> bool {
    ctx.send_host_or_drop(
        wl_registry::req::bind(registry_host_id, name, interface.name(), version, host_id),
        format_args!("internal bind {name}:{interface}"),
    )
}

/// Bind host global `name` as `host_id` for the proxy's own use.
///
/// # Returns
///
/// The session's end if the bind cannot be encoded, which a static
/// interface name never causes.
fn bind_internally(
    ctx: &mut Context,
    registry_host_id: HostId,
    name: u32,
    interface: Iface,
    version: u32,
    host_id: HostId,
) -> Result<(), Action> {
    if queue_internal_bind(ctx, registry_host_id, name, interface, version, host_id) {
        Ok(())
    } else {
        Err(ctx.fail(format_args!("Failed to bind host {interface} internally")))
    }
}

impl Singleton {
    /// The interface of the global it binds, and how: the one row of
    /// [`policy`] that binds it, which [`check_table`] holds to exist and
    /// be the only one.
    const fn row(self) -> (Iface, Internal) {
        let mut i = 0;
        while i < Iface::ALL.len() {
            if let Policy::Bind(internal) = policy(Iface::ALL[i]) {
                if internal.singleton as usize == self as usize {
                    return (Iface::ALL[i], internal);
                }
            }
            i += 1;
        }
        panic!("a singleton has no row in the table of globals")
    }

    /// The interface of the global it binds.
    pub(crate) const fn interface(self) -> Iface {
        self.row().0
    }

    /// Bind host global `name`, which the host advertises at `version`,
    /// through host registry `registry_host_id`, as the one binding.
    ///
    /// # Returns
    ///
    /// The session's end if the bind cannot be encoded.
    fn bind(
        self,
        ctx: &mut Context,
        registry_host_id: HostId,
        name: u32,
        version: u32,
    ) -> Result<(), Action> {
        let (interface, internal) = self.row();
        let bound_version = version.min(internal.ceiling);
        // The binding is the proxy's own: no guest object pairs with it.
        let host_id = ctx.shadow_table.create_host_only(interface, bound_version);
        ctx.globals
            .singletons
            .bind(self, binding(host_id, name, bound_version));
        bind_internally(
            ctx,
            registry_host_id,
            name,
            interface,
            bound_version,
            host_id,
        )?;
        log::debug!("Bound {interface} v{bound_version} internally (host_id={host_id})");
        match self {
            Self::Dmabuf => {
                // The host sends the binding its formats and modifiers, from
                // which a v3 or newer host's feedback is synthesized.
                crate::handler::linux_dmabuf::start_capability_discovery(ctx, name);
                if crate::handler::linux_dmabuf::can_synthesize_feedback(ctx, version) {
                    queue_internal_sync(ctx, InternalCallback::DmabufCapabilities { global: name });
                }
            }
            // Placement retries what a guest set up before the shell.
            Self::AuraShell => crate::handler::window_placement::shell_bound(ctx, bound_version),
            Self::KeyboardExtension
            | Self::Shm
            | Self::TextInputManager
            | Self::TextInputExtension => {}
        }
        Ok(())
    }
}

/// The binding of host object `host` from global `name` at `version`.
fn binding(host: HostId, name: u32, version: u32) -> Binding {
    Binding {
        host,
        name,
        version,
    }
}

/// Record host global `name` of `singleton`, which the host advertises at
/// `version`, as a candidate binding.
fn add_candidate(ctx: &mut Context, singleton: Singleton, name: u32, version: u32) {
    ctx.globals
        .singleton_candidates
        .insert(name, (singleton, version));
}

/// Bind `singleton` if the proxy has no binding of it: the lowest candidate
/// that a registry still lists and whose removal no registry has seen,
/// through the lowest registry that lists it.
///
/// # Returns
///
/// The session's end if the bind cannot be encoded.
fn ensure_bound(ctx: &mut Context, singleton: Singleton) -> Result<(), Action> {
    if ctx.globals.singletons.get(singleton).is_some() {
        return Ok(());
    }
    let candidate = ctx
        .globals
        .singleton_candidates
        .iter()
        .filter(|(_, &(candidate, _))| candidate == singleton)
        .filter_map(|(&name, &(_, version))| {
            if global_removal_seen(ctx, name) {
                return None;
            }
            let registry_host_id = ctx.globals.listing(name)?;
            Some((name, registry_host_id, version))
        })
        .min();
    match candidate {
        Some((name, registry_host_id, version)) => {
            singleton.bind(ctx, registry_host_id, name, version)
        }
        None => Ok(()),
    }
}

/// [`ensure_bound`] for every singleton, after a binding may have retired.
/// Only the retired binding can have a candidate to bind, so the order
/// they are visited in does not matter.
fn ensure_all_bound(ctx: &mut Context) -> Result<(), Action> {
    for iface in Iface::ALL {
        if let Policy::Bind(internal) = policy(iface) {
            ensure_bound(ctx, internal.singleton)?;
        }
    }
    Ok(())
}

/// Release the binding that host global `name` backs, if one does.
///
/// ChromiumOS destroys the internally bound proxy when its host registry
/// global disappears, and so does the proxy, which binds a replacement
/// global, if one is advertised, under a name of its own.
fn reset_internal_binding_for_global(ctx: &mut Context, name: u32) {
    let Some((singleton, binding)) = ctx.globals.singletons.bound_from(name) else {
        return;
    };
    // Each arm releases the binding first: what follows asks whether the
    // proxy still holds it.
    match singleton {
        // The text-input managers have no destructor, and text inputs still
        // create children from them after their globals go, so the proxy
        // keeps them for the connection, and never binds another.
        Singleton::TextInputManager | Singleton::TextInputExtension => {}
        Singleton::Dmabuf => {
            ctx.globals.singletons.release(singleton);
            // A registry global removal invalidates the name, not objects
            // already bound from it. Explicitly destroy our hidden binding
            // so the host does not retain a proxy until connection
            // teardown.
            ctx.send_host(zwp_linux_dmabuf_v1::req::destroy(binding.host));
            ctx.retire_destroyed_host_object(binding.host);
            clear_host_shm_dmabuf_formats(ctx);
            crate::handler::linux_dmabuf::maybe_reclaim_capabilities(ctx, name);
        }
        Singleton::Shm => {
            ctx.globals.singletons.release(singleton);
            ctx.shadow_table.retire_host(binding.host);
            clear_host_shm_wl_formats(ctx);
        }
        Singleton::KeyboardExtension => {
            ctx.globals.singletons.release(singleton);
            // `global_remove` invalidates only the manager global name. It
            // does not destroy `zcr_extended_keyboard_v1` children that
            // were already created with get_extended_keyboard; those
            // children have their own destructor and remain usable until
            // the corresponding wl_keyboard is released. Keep both maps and
            // the child dispatch registrations so queued peek_key events
            // remain routable after the manager disappears.
            ctx.shadow_table.retire_host(binding.host);
        }
        Singleton::AuraShell => {
            ctx.globals.singletons.release(singleton);
            // `global_remove` invalidates only the advertised global name.
            // Already created `zaura_surface` children have their own
            // protocol lifetime and remain usable until their owning
            // wl_surface is destroyed. Keep their mappings and per-object
            // version metadata so an existing surface can still receive
            // set_application_id after the manager global disappears. This
            // also avoids sending a release for a child that the host never
            // asked us to destroy.
            //
            // Release only the internally bound manager, matching
            // ChromiumOS' registry remover. New children cannot be created
            // until a replacement shell global is advertised and rebound.
            crate::handler::aura_objects::release_shell(ctx, binding);
        }
    }
}

/// Advertise host global `name` to the guest on the sending registry as
/// `interface` at `version`, in place of the host's advertisement.
fn advertise_synthetic_global(
    ctx: &mut Context,
    registry: HostId,
    name: u32,
    interface: Iface,
    version: u32,
) {
    ctx.globals
        .host_globals
        .insert(name, HostGlobal { interface, version });
    let visible = queue_synthetic_global_for_registry(ctx, registry, name, interface, version);
    record_registry_global_visibility(ctx, registry, name, visible);
}

/// Keep host global `name` of `interface`, which the proxy uses itself,
/// from the guest.
fn hide_global(ctx: &mut Context, registry: HostId, name: u32) {
    record_registry_global_visibility(ctx, registry, name, false);
}

fn record_registry_global_visibility(
    ctx: &mut Context,
    registry: HostId,
    name: u32,
    visible: bool,
) {
    set_registry_global_visibility(ctx, registry, name, visible);
}

/// Host registry `registry_host_id`'s current advertisement of global
/// `name` was `visible` to the guest, or not. Only a registry that lists
/// the name has an advertisement to classify.
fn set_registry_global_visibility(
    ctx: &mut Context,
    registry_host_id: HostId,
    name: u32,
    visible: bool,
) {
    if let Some(record) = ctx.globals.registries.get_mut(&registry_host_id) {
        record.set_visibility(name, visible);
    }
}

/// How the proxy serves an admitted guest bind.
#[derive(Debug)]
enum BindAdmission {
    /// Serve it as the interface's row says.
    Live(Serve),
    /// The registry advertised the global and then removed it, and the
    /// bind crossed the removal on the wire. libwayland-server makes this
    /// race harmless, so the proxy creates an inert guest object instead
    /// of ending the session.
    Inert,
}

/// Decide whether the guest may bind global `name` as `interface` at
/// `requested_version` through the registry that sent the request, whose
/// host registry is `registry_host_id`: a global is advertised
/// independently on each `wl_registry`, which the proxy tracks by its host
/// registry.
///
/// # Returns
///
/// How to serve the bind, or `None` to reject it as a protocol error.
fn admit_guest_bind(
    ctx: &Context,
    registry_host_id: HostId,
    name: u32,
    interface: Iface,
    requested_version: u32,
) -> Option<BindAdmission> {
    // Once one registry has observed global_remove, a bind through it never
    // reaches the host, even while another registry still lists the global.
    // A bind through a registry that has not seen it yet is forwarded
    // below: that relies on the host keeping a removed global bindable
    // until wl_global_destroy, as Exo does by deferring it after
    // wl_global_remove. A host that destroyed it at once would answer the
    // bind with INVALID_OBJECT, as it would a native client's.
    if let Some(removal) = ctx
        .globals
        .registries
        .get(&registry_host_id)
        .and_then(|record| record.removal(name))
    {
        let inert = interface == INERT_BIND_INTERFACE
            && removal
                .inert_bind_version
                .is_some_and(|version| (1..=version).contains(&requested_version));
        return inert.then_some(BindAdmission::Inert);
    }

    let global = ctx.globals.host_globals.get(&name)?;
    if let Some(record) = ctx.globals.registries.get(&registry_host_id) {
        if !record.lists(name) {
            return None;
        }
    }

    // A synthetic global is recorded under the name of the host global
    // that carries it. Checking the advertised version here also prevents
    // a client from forwarding a bind for a protocol version that the
    // proxy did not expose.
    let version_is_advertised = global.interface == interface
        && requested_version > 0
        && requested_version <= global.version;
    if !version_is_advertised {
        return None;
    }
    Some(BindAdmission::Live(policy(interface).serve()?))
}

/// Record the sending registry's advertisement of host global `name`.
///
/// A name is one global for the connection's life: libwayland-server
/// numbers each global anew and never reuses a name, and sends a registry
/// each global once. A name the registry has advertised already, or one
/// whose removal any registry has seen, breaks the protocol, and ends the
/// session.
///
/// # Returns
///
/// The action for the event if the proxy handles it no further: `Drop`
/// for a malformed advertisement, or the session's end.
fn accept_global(
    ctx: &mut Context,
    registry: HostId,
    name: u32,
    interface: &str,
    version: u32,
) -> Result<(), Action> {
    if version == 0 {
        log::warn!(
            "Ignoring malformed Wayland global {}:{} with version 0",
            name,
            interface
        );
        return Err(Action::Drop);
    }
    let advertised = ctx
        .globals
        .registries
        .get(&registry)
        .is_some_and(|record| record.advertised(name));
    if advertised || global_removal_seen(ctx, name) {
        return Err(ctx.fail(format_args!(
            "The host advertised global name {name} ({interface}) again in registry \
             {registry}, which breaks the protocol"
        )));
    }
    ctx.globals
        .registries
        .entry(registry)
        .or_default()
        .list(name);
    Ok(())
}

/// A host global the proxy binds for its own use, as `internal` says: the
/// guest is offered what it carries under its name, and it is a candidate
/// for the proxy's binding. One the proxy does not use is hidden.
fn internal_global(
    ctx: &mut Context,
    registry: HostId,
    name: u32,
    version: u32,
    internal: Internal,
) -> Action {
    if !(internal.binds)(ctx, version) {
        log::debug!(
            "Ignoring {} v{version}, which the proxy does not use",
            internal.singleton.interface()
        );
        hide_global(ctx, registry, name);
        return Action::Drop;
    }
    match internal.carries {
        Carries::Global {
            iface,
            version: carried_version,
            offered,
        } if offered(ctx) => {
            advertise_synthetic_global(ctx, registry, name, iface, carried_version)
        }
        Carries::Global { .. } => hide_global(ctx, registry, name),
        Carries::Dmabuf => offer_dmabuf_global(ctx, registry, name, version),
    }
    // The host compositor sends the same global list to every registry,
    // and the proxy has one binding per connection.
    add_candidate(ctx, internal.singleton, name, version);
    // A failed bind ends the session; the host's own advertisement is never
    // forwarded.
    ensure_bound(ctx, internal.singleton)
        .err()
        .unwrap_or(Action::Drop)
}

/// Offer the guest the linux-dmabuf global the host advertises as `name`
/// at `version`, which the proxy binds: with GPU access, at once or once
/// the capabilities of its binding arrive, and hidden otherwise.
fn offer_dmabuf_global(ctx: &mut Context, registry: HostId, name: u32, version: u32) {
    if !ctx.settings().gpu_accel {
        // Virtwl uses this object only to learn the formats that can
        // be advertised by the synthetic wl_shm global. Keep the
        // linux-dmabuf global hidden from the guest, matching
        // ChromiumOS's separate --enable-linux-dmabuf switch.
        hide_global(ctx, registry, name);
        return;
    }
    // A v3 host still has enough legacy format/modifier information
    // for our synthetic v4 feedback object. Advertise v4 to the
    // guest while clamping only the internal host bind to v3.
    let can_synthesize_feedback =
        crate::handler::linux_dmabuf::can_synthesize_feedback(ctx, version);
    let client_version = if can_synthesize_feedback {
        GUEST_DMABUF_VERSION
    } else {
        if version >= LINUX_DMABUF_CAPABILITY_VERSION && ctx.virtgpu().is_none() {
            log::warn!(
                "No virtio-gpu render node; exposing linux-dmabuf v3 without synthetic \
                 device feedback"
            );
        }
        version.min(LINUX_DMABUF_CAPABILITY_VERSION)
    };
    // One host dmabuf object is shared for capability discovery, and a
    // guest global for it waits until the binding of its own name has the
    // capabilities.
    let capability_ready = crate::handler::linux_dmabuf::capabilities_ready(ctx, name);
    if can_synthesize_feedback && !capability_ready {
        defer_synthetic_dmabuf_global(ctx, registry, name, client_version);
    } else if !can_synthesize_feedback
        || crate::handler::linux_dmabuf::has_synthetic_feedback_formats(ctx, name)
    {
        advertise_synthetic_global(ctx, registry, name, Iface::ZwpLinuxDmabufV1, client_version);
    } else {
        hide_global(ctx, registry, name);
    }
}

/// A host global of `iface` the proxy forwards, at `advertised_version`
/// ([`offered_version`]).
fn forwarded_global(
    ctx: &mut Context,
    registry: HostId,
    name: u32,
    iface: Iface,
    version: u32,
    advertised_version: u32,
) -> Action {
    if advertised_version != version {
        advertise_synthetic_global(ctx, registry, name, iface, advertised_version);
        return Action::Drop;
    }
    ctx.globals.host_globals.insert(
        name,
        HostGlobal {
            interface: iface,
            version: advertised_version,
        },
    );
    record_registry_global_visibility(ctx, registry, name, true);
    Action::Forward
}

/// A host global the proxy keeps from the guest.
fn hidden_global(ctx: &mut Context, registry: HostId, name: u32, interface: &str) -> Action {
    if interface.contains(char::REPLACEMENT_CHARACTER) {
        log::debug!("Hiding global {name}, whose interface name {interface:?} is not UTF-8");
    }
    hide_global(ctx, registry, name);
    Action::Drop
}

impl wl_registry::WlRegistryHandler for Handlers {
    /// A host global: the proxy binds it for itself, advertises a global
    /// of its own in its place, forwards it, or hides it, by its
    /// interface.
    fn on_global(
        ctx: &mut Context,
        sender: EventSender,
        name: u32,
        interface: &String,
        version: u32,
    ) -> Action {
        let registry = sender.host;
        if let Err(action) = accept_global(ctx, registry, name, interface, version) {
            return action;
        }
        let Some(iface) = Iface::from_name(interface) else {
            return hidden_global(ctx, registry, name, interface);
        };
        match policy(iface) {
            Policy::Forward {
                ceiling,
                enabled,
                serve,
            } if enabled(ctx.settings()) => {
                let offered = offered_version(ceiling, serve, version);
                forwarded_global(ctx, registry, name, iface, version, offered)
            }
            Policy::Forward { .. } | Policy::Synthetic(_) | Policy::Hide => {
                hidden_global(ctx, registry, name, interface)
            }
            Policy::Bind(internal) => internal_global(ctx, registry, name, version, internal),
        }
    }

    fn on_global_remove(ctx: &mut Context, sender: EventSender, name: u32) -> Action {
        let registry_id = sender.host;
        let Some(record) = ctx.globals.registries.get(&registry_id) else {
            return Action::Drop;
        };
        if !record.lists(name) {
            return Action::Drop;
        }
        let was_visible = record
            .visibility(name)
            .unwrap_or_else(|| ctx.globals.host_globals.contains_key(&name));
        ctx.globals
            .pending_dmabuf
            .retain(|pending| !(pending.registry_host_id == registry_id && pending.name == name));
        // A bound wl_output remains a valid Wayland object after its global
        // advertisement disappears, but it must stop feeding
        // compositor-owned placement, and its host-only zaura_output is
        // released now (see `OutputRegistry::remove_global`).
        for aura_output in ctx.outputs.remove_global(name) {
            crate::handler::aura_objects::release(
                ctx,
                crate::handler::aura_objects::AuraChild::OUTPUT,
                aura_output,
            );
        }
        crate::handler::linux_dmabuf::maybe_reclaim_capabilities(ctx, name);
        // Keep the version of a removed output the guest saw, so a bind that
        // crossed this removal is told apart from a bind of a name it never
        // saw.
        let inert_bind_version = ctx
            .globals
            .host_globals
            .get(&name)
            .filter(|global| was_visible && global.interface == INERT_BIND_INTERFACE)
            .map(|global| global.version);
        if let Some(record) = ctx.globals.registries.get_mut(&registry_id) {
            record.remove(name, inert_bind_version);
        }
        // Host registries are independent Wayland objects, and each gets
        // its own global_remove. The connection-wide metadata stays until
        // every registry that lists the global has seen it.
        ctx.globals.removed_globals.insert(name);
        if !ctx.globals.still_seen(name) {
            retire_unseen_global(ctx, name);
        }
        // Only globals previously exposed to the guest should generate a
        // matching removal event. Hidden/internal globals were never sent to
        // the guest and must not leak lifecycle notifications.
        if was_visible {
            Action::Forward
        } else {
            Action::Drop
        }
    }

    /// A guest bind, which only this handler knows how to serve: the
    /// generated code forwards nothing for it
    /// (`wayland_codegen::generator::HAND_WRITTEN_REQUESTS`).
    fn on_bind(ctx: &mut Context, sender: RequestSender, name: u32, id: &(String, u32, GuestId)) {
        let (requested, version, guest_new_id) = (&id.0, id.1, id.2);
        // The proxy implements no wl_registry itself.
        let Some(registry_host_id) = sender.host_id() else {
            let _ = ctx.fail(format_args!(
                "Registry {} has no host registry",
                sender.guest
            ));
            return;
        };

        // The object table keeps the interface's generated name. One no
        // vendored protocol defines is never advertised, so it is no global
        // the guest may bind either.
        let admission = Iface::from_name(requested).and_then(|interface| {
            Some((
                interface,
                admit_guest_bind(ctx, registry_host_id, name, interface, version)?,
            ))
        });
        let (interface, serve) = match admission {
            Some((interface, BindAdmission::Live(serve))) => (interface, serve),
            Some((interface, BindAdmission::Inert)) => {
                log::debug!(
                    "Bind of removed global {}:{} v{} crossed its removal; guest object {} is inert",
                    name,
                    interface,
                    version,
                    guest_new_id
                );
                // A local object: it never reaches the host, and its
                // destructor completes in the proxy.
                ctx.shadow_table
                    .create_local(guest_new_id, interface, version);
                return;
            }
            None => {
                error!(
                    "Rejecting bind for unadvertised, unsupported, or too-new global {}:{} v{}",
                    name, requested, version
                );
                let _ = ctx.reject(
                    sender.guest,
                    wl_display::ERROR_INVALID_OBJECT,
                    format!(
                        "invalid bind for global {} (interface {}, version {})",
                        name, requested, version
                    ),
                );
                return;
            }
        };

        match serve {
            Serve::Paired { host_ceiling, then } => {
                let host_version = version.min(host_ceiling);
                let bound = bind_for_guest(
                    ctx,
                    registry_host_id,
                    name,
                    interface,
                    (version, host_version),
                    guest_new_id,
                );
                if let (Some(host_new_id), Some(then)) = (bound, then) {
                    after_bind(ctx, then, name, guest_new_id, host_new_id, host_version);
                }
            }
            Serve::Local => {
                ctx.shadow_table
                    .create_local(guest_new_id, interface, version);
            }
            // Mandatory formats are sent at once, optional ones only once
            // the host reports it can import them, as ChromiumOS Sommelier
            // does, so a guest never selects a format the host cannot
            // import. The host's wl_shm was bound when its global was
            // advertised.
            Serve::Shm => register_guest_shm(ctx, guest_new_id, version, name),
            Serve::GtkShell => {
                crate::handler::gtk_shell::create_shell(ctx, guest_new_id, version);
                queue_internal_sync(
                    ctx,
                    InternalCallback::GtkShellCapabilities {
                        gtk_shell: guest_new_id,
                    },
                );
            }
        }
    }
}

/// Bind host global `name` of `interface` through host registry
/// `registry_host_id` for guest object `guest_new_id`, at the guest's and
/// the host's version.
///
/// # Returns
///
/// The host object, or `None` if the bind cannot be encoded, which ends the
/// session.
fn bind_for_guest(
    ctx: &mut Context,
    registry_host_id: HostId,
    name: u32,
    interface: Iface,
    (guest_version, host_version): (u32, u32),
    guest_new_id: GuestId,
) -> Option<HostId> {
    let host_new_id =
        ctx.shadow_table
            .create_paired(guest_new_id, interface, guest_version, host_version);
    if !queue_internal_bind(
        ctx,
        registry_host_id,
        name,
        interface,
        host_version,
        host_new_id,
    ) {
        ctx.abandon(guest_new_id);
        let _ = ctx.fail(format_args!(
            "Failed to bind host {interface} for the guest"
        ));
        return None;
    }
    Some(host_new_id)
}

/// Do what `then` says once the guest's bind of global `name`, as
/// `guest`, paired with host object `host` of `version`, is queued, so that
/// a failed bind has nothing of it to undo.
fn after_bind(
    ctx: &mut Context,
    then: AfterBind,
    name: u32,
    guest: GuestId,
    host: HostId,
    version: u32,
) {
    match then {
        AfterBind::Output => {
            ctx.outputs.bind(host, version, name);
            // Placement's host-only Aura output metadata (scale factors
            // and insets).
            crate::handler::window_placement::output_bound(ctx, host);
        }
        AfterBind::Seat => {
            // A text input whose seat had no host object can activate now.
            let seat = crate::ime::SeatId(guest);
            crate::handler::text_input::ime(ctx, crate::ime::Input::SeatBound(seat));
        }
        AfterBind::DmabufFactory => {
            crate::handler::linux_dmabuf::factory_bound(ctx, guest, name);
        }
    }
}

/// Forget host registry `registry_host_id`, which ended (see
/// `crate::state::lifecycle`): what it was advertised, and its hold on
/// dma-buf capabilities.
///
/// A global whose removal another registry has seen waits for every
/// registry that still lists it (see `on_global_remove`). If this was the
/// last one, it will never see the removal, so the global is retired now.
pub(crate) fn forget_registry(ctx: &mut Context, registry_host_id: HostId) {
    let record = ctx
        .globals
        .registries
        .remove(&registry_host_id)
        .unwrap_or_default();
    ctx.globals
        .pending_dmabuf
        .retain(|pending| pending.registry_host_id != registry_host_id);
    for name in record.listed() {
        if global_removal_seen(ctx, name) && !ctx.globals.still_seen(name) {
            retire_unseen_global(ctx, name);
        }
    }
    for name in record.names() {
        crate::handler::linux_dmabuf::maybe_reclaim_capabilities(ctx, name);
    }
    // A candidate no registry lists any more can never be bound.
    let unlisted: Vec<u32> = ctx
        .globals
        .singleton_candidates
        .keys()
        .copied()
        .filter(|&name| !ctx.globals.still_seen(name))
        .collect();
    for name in unlisted {
        ctx.globals.singleton_candidates.remove(&name);
    }
}

impl wl_fixes::WlFixesHandler for Handlers {
    fn on_destroy_registry(
        ctx: &mut Context,
        _sender: RequestSender,
        registry: RequestObject,
    ) -> Action {
        // wl_fixes destroys the registry passed as an object argument rather
        // than its own request sender. Reserve that guest/host mapping until
        // wl_display.delete_id acknowledges the host-side destruction.
        ctx.guest_destroyed(registry.guest);
        Action::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::{
        offered_version, policy, should_bind_internal_dmabuf, Policy,
        LINUX_DMABUF_CAPABILITY_VERSION, WL_SHM_VERSION, ZAURA_SHELL_MIN_VERSION,
    };
    use crate::handler::Handlers;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
    use crate::protocols::gtk::{gtk_shell1, gtk_surface1};
    use crate::protocols::keyboard_extension_unstable_v1::{
        zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
    };
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::{
        wl_callback, wl_display, wl_fixes, wl_keyboard, wl_output, wl_registry, wl_shm, wl_surface,
    };
    use crate::protocols::{AnyEvent, AnyRequest, Iface};
    use crate::proxy::harness::{Object, Session};
    use crate::state::{Context, GuestId, HostId, Singleton};
    use crate::wire::{Action, Outgoing, WL_DISPLAY_ID};

    /// How many IDs the guest skips before its first object. The proxy
    /// allocates host IDs densely from the bottom, so without the gap a
    /// registry could have the same ID on both sides, and looking up the
    /// wrong one would pass.
    const GUEST_ID_LEAD: u32 = 10;

    /// One of the guest's two registries.
    #[derive(Clone, Copy, Debug)]
    enum Registry {
        A,
        B,
    }
    use Registry::{A, B};

    /// What a table test has the host or the guest do to the registries.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// The host advertises global `name` of `interface` at `version` on
        /// the registry.
        Global(Registry, u32, &'static str, u32),
        /// The host removes global `name` from the registry.
        Remove(Registry, u32),
        /// The guest destroys the registry through `wl_fixes`.
        Destroy(Registry),
    }
    use Step::{Destroy, Global, Remove};

    /// A whole session for registry tests: the guest's registries [`A`]
    /// (the session's [`Session::registry`]) and [`B`], and the
    /// `wl_fixes` it destroys them with, which the host advertised on `A`
    /// as global 1.
    struct Registries {
        s: Session,
        a: Object,
        b: Object,
        fixes: Object,
    }

    impl Registries {
        /// A session without GPU access.
        async fn new() -> Self {
            Self::with_context(Context::new_for_test(false, false, vec![])).await
        }

        /// A session with GPU access, which the proxy needs to offer the
        /// host's linux-dmabuf, and with synthetic dma-buf feedback if
        /// `feedback`.
        async fn gpu(feedback: bool) -> Self {
            let mut ctx = Context::new_for_test(true, false, vec![]);
            // Stands in for a virtio-gpu render node, which the proxy
            // synthesizes feedback for and a test cannot have.
            ctx.dmabuf.set_feedback_available_for_test(feedback);
            Self::with_context(ctx).await
        }

        /// A session whose window placement uses Aura.
        async fn aura_placement() -> Self {
            let placement = PlacementConfig::active_for_test(&[]);
            Self::with_context(Context::with_placement_for_test(placement)).await
        }

        async fn with_context(ctx: Context) -> Self {
            let mut s = Session::with_context(ctx);
            for _ in 0..GUEST_ID_LEAD {
                s.new_id();
            }
            let a = s.registry().await;
            let b = s.new_registry().await;
            let fixes = s.bind_paired(wl_fixes::NAME, wl_fixes::VERSION).await;
            assert_ne!(a.id, a.host);
            Self { s, a, b, fixes }
        }

        fn of(&self, registry: Registry) -> Object {
            match registry {
                A => self.a,
                B => self.b,
            }
        }

        /// The host advertises global `name` of `interface` at `version`
        /// on `registry`.
        async fn global(&mut self, registry: Registry, name: u32, interface: &str, version: u32) {
            let registry = self.of(registry);
            self.s.global(registry, name, interface, version).await;
        }

        /// The host removes global `name` from `registry`.
        async fn remove(&mut self, registry: Registry, name: u32) {
            let registry = self.of(registry);
            self.s.global_remove(registry, name).await;
        }

        /// The guest binds global `name` as `interface` at `version`
        /// through `registry`; returns its new object.
        async fn bind(
            &mut self,
            registry: Object,
            name: u32,
            interface: &str,
            version: u32,
        ) -> u32 {
            let id = self.s.new_id();
            let bind = wl_registry::req::bind(registry.id, name, interface, version, id).unwrap();
            self.s.guest([bind]).await;
            id
        }

        /// [`Self::bind`] of a global the proxy forwards, which the host
        /// gets alone through `registry`, at the same version; returns the
        /// guest's object and the host's.
        async fn bind_paired(
            &mut self,
            registry: Object,
            name: u32,
            interface: &str,
            version: u32,
        ) -> Object {
            let id = self.bind(registry, name, interface, version).await;
            let host = self.s.host_received();
            let [forward] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let bound = bound(forward);
            assert_eq!(
                host,
                [(registry.host, bind_request(name, interface, version, bound))]
            );
            Object { id, host: bound }
        }

        /// [`Self::bind`], which the proxy refuses with an
        /// `INVALID_OBJECT` error on `registry` that ends the session; the
        /// host gets nothing.
        async fn bind_fatal(&mut self, registry: Object, name: u32, interface: &str, version: u32) {
            let id = self.s.new_id();
            let bind = wl_registry::req::bind(registry.id, name, interface, version, id).unwrap();
            self.s.guest_fatal([bind]).await;
            assert_eq!(
                self.s.guest_received_and_error(),
                (vec![], (registry.id, wl_display::ERROR_INVALID_OBJECT))
            );
            assert_eq!(self.s.host_received(), []);
        }

        /// The guest destroys `registry` through `wl_fixes`, which the
        /// proxy forwards, and the host acknowledges it with `delete_id`,
        /// which the guest gets; returns what else the proxy sent the host.
        async fn destroy(&mut self, registry: Registry) -> Vec<(u32, AnyRequest)> {
            let registry = self.of(registry);
            let destroy = wl_fixes::req::destroy_registry(self.fixes.id, registry.id);
            self.s.guest([destroy]).await;
            let mut host = self.s.host_received();
            let forward = |request: &(u32, AnyRequest)| {
                let destroy = wl_fixes::Request::DestroyRegistry {
                    registry: registry.host,
                };
                *request == (self.fixes.host, destroy.into())
            };
            let at = host
                .iter()
                .position(forward)
                .unwrap_or_else(|| panic!("the host got no destroy_registry: {host:?}"));
            host.remove(at);
            self.s
                .host([wl_display::evt::delete_id(WL_DISPLAY_ID, registry.host)])
                .await;
            let deleted = wl_display::Event::DeleteId { id: registry.id };
            assert_eq!(self.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
            host
        }

        /// Run `step`; returns what the proxy sent the host for it. What
        /// the guest gets is kept.
        async fn run(&mut self, step: Step) -> Vec<(u32, AnyRequest)> {
            match step {
                Global(registry, name, interface, version) => {
                    self.global(registry, name, interface, version).await;
                }
                Remove(registry, name) => self.remove(registry, name).await,
                Destroy(registry) => return self.destroy(registry).await,
            }
            self.s.host_received()
        }
    }

    /// A `wl_registry.global` event.
    fn global(name: u32, interface: &str, version: u32) -> AnyEvent {
        wl_registry::Event::Global {
            name,
            interface: interface.to_string(),
            version,
        }
        .into()
    }

    /// A `wl_registry.global_remove` event.
    fn global_remove(name: u32) -> AnyEvent {
        wl_registry::Event::GlobalRemove { name }.into()
    }

    fn bind_request(name: u32, interface: &str, version: u32, id: u32) -> AnyRequest {
        wl_registry::Request::Bind {
            name,
            id: (interface.to_string(), version, id),
        }
        .into()
    }

    /// The host object that `request`, a bind, creates.
    fn bound(request: &(u32, AnyRequest)) -> u32 {
        match request {
            (_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })) => id.2,
            _ => panic!("{request:?} is no bind"),
        }
    }

    /// The `wl_display.sync` of callback `callback`.
    fn sync(callback: u32) -> (u32, AnyRequest) {
        (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into())
    }

    /// A format the proxy can offer in synthetic linux-dmabuf feedback:
    /// `XRGB8888` with the linear modifier.
    const XRGB8888_LINEAR: (u32, u64) = (0x3432_5258, 0);

    /// The proxy's binding of host linux-dmabuf global `name` through
    /// `registry`, at v3, which asks for the binding's capabilities with a
    /// sync: `requests`, which are all the host got. Returns the binding
    /// and the sync's callback.
    fn dmabuf_binding(requests: &[(u32, AnyRequest)], registry: Object, name: u32) -> (u32, u32) {
        let [bind, (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
            requests
        else {
            panic!("the host got {requests:?}, not a bind and a sync");
        };
        let dmabuf = bound(bind);
        let expected = bind_request(name, zwp_linux_dmabuf_v1::NAME, 3, dmabuf);
        assert_eq!(*bind, (registry.host, expected));
        (dmabuf, *callback)
    }

    /// The host's answer to capability sync `callback` of linux-dmabuf
    /// binding `dmabuf`: its `formats`, the callback's `done`, and the
    /// callback's `delete_id`.
    fn capabilities(dmabuf: u32, callback: u32, formats: &[(u32, u64)]) -> Vec<Outgoing<u32>> {
        let formats = formats.iter().map(|&(format, modifier)| {
            let (hi, lo) = ((modifier >> 32) as u32, modifier as u32);
            zwp_linux_dmabuf_v1::evt::modifier(dmabuf, format, hi, lo)
        });
        formats
            .chain([
                wl_callback::evt::done(callback, 0),
                wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
            ])
            .collect()
    }

    /// The guest's `id`, a `wl_shm` or `gtk_shell1` it just bound, is the
    /// proxy's own: the host gets no bind for it, and the guest gets what
    /// the proxy serves it, `wl_shm`'s mandatory formats, or `gtk_shell1`'s
    /// capabilities once the host has answered the proxy's sync.
    async fn assert_served_locally(s: &mut Session, id: u32, interface: &str) {
        match interface {
            wl_shm::NAME => {
                assert_eq!(s.host_received(), [], "no second host bind");
                assert_eq!(
                    s.guest_received(),
                    [
                        (id, wl_shm::Event::Format { format: 0 }.into()),
                        (id, wl_shm::Event::Format { format: 1 }.into()),
                    ],
                    "only the mandatory formats before the host's capabilities"
                );
            }
            gtk_shell1::NAME => {
                let host = s.host_received();
                let [(WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
                    host[..]
                else {
                    panic!("the host got {host:?}, not one sync");
                };
                assert_eq!(s.guest_received(), []);
                s.host([
                    wl_callback::evt::done(callback, 0),
                    wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
                ])
                .await;
                let capabilities = gtk_shell1::Event::Capabilities { capabilities: 0 };
                assert_eq!(s.guest_received(), [(id, capabilities.into())]);
            }
            _ => panic!("the proxy serves no {interface}"),
        }
    }

    /// The proxy binds a host global it uses itself at the host's version,
    /// but never newer than its row's ceiling: a keyboard extension newer
    /// than the XML is bound at its v2, which reports the keys the IME
    /// consumed (`peek_key`).
    #[tokio::test]
    async fn an_internal_binding_is_capped_at_its_ceiling() {
        let interface = zcr_keyboard_extension_v1::NAME;
        for (host_version, version) in [(1, 1), (2, 2), (99, 2)] {
            let mut r = Registries::new().await;
            r.global(A, 10, interface, host_version).await;
            let host = r.s.host_received();
            let [request] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let binding = bound(request);
            assert_eq!(
                host,
                [(r.a.host, bind_request(10, interface, version, binding))],
                "v{host_version}"
            );
            assert_eq!(r.s.guest_received(), [], "v{host_version}");
        }
    }

    #[test]
    fn virtwl_dmabuf_capability_requires_an_internal_bind_without_gpu() {
        assert!(
            should_bind_internal_dmabuf(false, true),
            "a virtwl channel with dmabuf support must be probed and bound even \
             when the guest-facing GPU path is disabled"
        );
    }

    /// A registry destroyed through `wl_fixes` takes the linux-dmabuf
    /// global it was to be advertised with it: once the capabilities
    /// arrive, no registry gets it. The session's check that no state
    /// outlives its object pins that the proxy drops the deferred
    /// advertisement with the registry.
    #[tokio::test]
    async fn wl_fixes_destroy_registry_removes_deferred_dmabuf_advertisements() {
        let mut r = Registries::gpu(true).await;
        r.global(B, 7, zwp_linux_dmabuf_v1::NAME, 4).await;
        let (dmabuf, callback) = dmabuf_binding(&r.s.host_received(), r.b, 7);
        assert_eq!(r.s.guest_received(), [], "the global waits for its formats");

        assert_eq!(r.destroy(B).await, []);

        let formats = capabilities(dmabuf, callback, &[XRGB8888_LINEAR]);
        r.s.host(formats).await;
        assert_eq!(r.s.guest_received(), []);
        assert_eq!(r.s.host_received(), []);
    }

    /// Each registry gets its own synthetic global, under the name its host
    /// registry gave it, and every such name is bindable.
    #[tokio::test]
    async fn synthetic_globals_are_sent_to_each_guest_registry() {
        for (interface, version, bound_version, advertised) in [
            ("wl_shm", 2, 1, wl_shm::NAME),
            ("zaura_shell", 38, 38, gtk_shell1::NAME),
        ] {
            for name_on_b in [7, 8] {
                let mut r = Registries::new().await;
                r.global(A, 7, interface, version).await;
                r.global(B, name_on_b, interface, version).await;
                assert_eq!(
                    r.s.guest_received(),
                    [
                        (r.a.id, global(7, advertised, 1)),
                        (r.b.id, global(name_on_b, advertised, 1)),
                    ],
                    "{interface} as {name_on_b} on B"
                );
                let host = r.s.host_received();
                let [binding] = &host[..] else {
                    panic!("the host got {host:?}, not one bind");
                };
                let binding = bound(binding);
                assert_eq!(
                    host,
                    [(r.a.host, bind_request(7, interface, bound_version, binding))],
                    "{interface} is bound once"
                );

                let id = r.bind(r.b, name_on_b, advertised, 1).await;
                assert_served_locally(&mut r.s, id, advertised).await;
            }
        }
    }

    /// The proxy serves the `gtk_shell1` it advertises for `zaura_shell`
    /// itself, and sends its capabilities once the host has answered a
    /// sync.
    #[tokio::test]
    async fn a_gtk_shell_bind_is_served_locally() {
        let mut r = Registries::new().await;
        r.global(A, 7, zaura_shell::NAME, 38).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(7, "gtk_shell1", 1))]);
        assert_eq!(r.s.host_received().len(), 1, "the proxy binds the shell");

        let gtk_shell = r.bind(r.a, 7, gtk_shell1::NAME, 1).await;
        assert_served_locally(&mut r.s, gtk_shell, gtk_shell1::NAME).await;
    }

    /// The proxy binds each global it uses itself at the newest version
    /// both it and the host support, and keeps each binding's host ID from
    /// reuse: the host, which checks every new object, gets the guest's
    /// next objects under other IDs.
    #[tokio::test]
    async fn internal_bindings_are_capped_and_reserved_from_host_id_reuse() {
        let mut r = Registries::new().await;
        // Offered at v99, each is bound at the newest version the proxy
        // supports: the vendored XML's, or wl_shm's v1.
        let bindings = [
            (
                11,
                "zwp_text_input_manager_v1",
                zwp_text_input_manager_v1::VERSION,
            ),
            (
                12,
                "zcr_text_input_extension_v1",
                zcr_text_input_extension_v1::VERSION,
            ),
            (
                13,
                "zcr_keyboard_extension_v1",
                zcr_keyboard_extension_v1::VERSION,
            ),
            (14, "wl_shm", 1),
        ];
        for (name, interface, _) in bindings {
            r.global(A, name, interface, 99).await;
        }

        let host = r.s.host_received();
        let ids = Vec::from_iter(host.iter().map(bound));
        let binds = bindings.iter().zip(&ids);
        assert_eq!(
            host,
            Vec::from_iter(binds.map(|(&(name, interface, version), &id)| {
                (r.a.host, bind_request(name, interface, version, id))
            }))
        );
        assert_eq!(
            r.s.guest_received(),
            [
                (
                    r.a.id,
                    global(
                        11,
                        zwp_text_input_manager_v3::NAME,
                        zwp_text_input_manager_v3::VERSION
                    )
                ),
                (r.a.id, global(14, wl_shm::NAME, 1)),
            ]
        );
        let surface = r.s.surface().await;
        assert!(!ids.contains(&surface.host), "{ids:?} stay reserved");

        // Offered at v1, each is bound at v1.
        for (name, interface, _) in bindings {
            let mut r = Registries::new().await;
            r.global(A, name, interface, 1).await;
            let host = r.s.host_received();
            let [binding] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let binding = bound(binding);
            assert_eq!(
                host,
                [(r.a.host, bind_request(name, interface, 1, binding))],
                "{interface}"
            );
            r.s.guest_received();
        }
    }

    /// Without synthetic feedback the guest gets the host's linux-dmabuf at
    /// once, capped at v3, the version the proxy binds to collect legacy
    /// format modifiers.
    #[tokio::test]
    async fn a_dmabuf_without_synthetic_feedback_is_offered_and_bound_at_v3() {
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        for version in [4, 5, 99] {
            let mut r = Registries::gpu(false).await;
            r.global(A, 10, dmabuf, version).await;

            let host = r.s.host_received();
            let [binding] = &host[..] else {
                panic!("the host got {host:?}, not one bind and no sync");
            };
            let binding = bound(binding);
            assert_eq!(
                host,
                [(r.a.host, bind_request(10, dmabuf, 3, binding))],
                "v{version}"
            );
            assert_eq!(r.s.guest_received(), [(r.a.id, global(10, dmabuf, 3))]);
            r.bind_paired(r.a, 10, dmabuf, 3).await;
            r.bind_fatal(r.a, 10, dmabuf, 4).await;
        }
    }

    /// With synthetic feedback the guest's linux-dmabuf global waits for
    /// the capabilities of its binding, and stays hidden if the host offers
    /// no format the proxy supports: its removal does not reach the guest
    /// either.
    #[tokio::test]
    async fn a_synthetic_dmabuf_global_waits_for_its_capabilities() {
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        for (formats, offered) in [(&[XRGB8888_LINEAR][..], Some(4)), (&[], None)] {
            let mut r = Registries::gpu(true).await;
            r.global(A, 10, dmabuf, 4).await;
            let (binding, callback) = dmabuf_binding(&r.s.host_received(), r.a, 10);
            assert_eq!(r.s.guest_received(), []);

            r.s.host(capabilities(binding, callback, formats)).await;
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(offered.map(|version| (r.a.id, global(10, dmabuf, version))))
            );

            r.remove(A, 10).await;
            let destroy = zwp_linux_dmabuf_v1::Request::Destroy {};
            assert_eq!(r.s.host_received(), [(binding, destroy.into())]);
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(offered.map(|_| (r.a.id, global_remove(10))))
            );
        }
    }

    /// A global of version 0 is malformed; the proxy drops it.
    #[tokio::test]
    async fn a_zero_version_global_is_dropped() {
        let mut r = Registries::new().await;
        r.global(A, 10, "wl_seat", 0).await;
        assert_eq!(r.s.guest_received(), []);
        assert_eq!(r.s.host_received(), []);
        r.bind_fatal(r.a, 10, "wl_seat", 1).await;
    }

    /// A name is one global for the connection's life: libwayland-server
    /// numbers each global anew, advertises it to a registry once, and
    /// advertises no global it removed. A host that advertises a name
    /// again breaks the protocol, which ends the session.
    #[tokio::test]
    async fn a_name_advertised_again_ends_the_session() {
        for (case, setup) in [
            ("listed by the registry", &[Global(A, 7, "wl_seat", 5)][..]),
            (
                "removed from the registry",
                &[Global(A, 7, "wl_seat", 5), Remove(A, 7)],
            ),
            (
                "removed from a registry that alone listed it",
                &[Global(B, 7, "wl_seat", 5), Remove(B, 7)],
            ),
            (
                "removed from a registry, while another lists it",
                &[
                    Global(B, 7, "wl_seat", 5),
                    Global(A, 7, "wl_seat", 5),
                    Remove(B, 7),
                ],
            ),
            (
                "of a global the proxy drops, removed",
                &[Global(B, 7, "zz_unknown_v1", 1), Remove(B, 7)],
            ),
            (
                "of a singleton the proxy bound, removed",
                &[Global(B, 7, "wl_shm", 1), Remove(B, 7)],
            ),
        ] {
            let mut r = Registries::new().await;
            for &step in setup {
                r.run(step).await;
            }
            r.s.guest_received();

            let again = wl_registry::evt::global(r.a.host, 7, "wl_seat", 5).unwrap();
            r.s.host_fatal([again]).await;
            assert_eq!(
                r.s.guest_received_and_error(),
                (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
                "{case}"
            );
            assert_eq!(r.s.host_received(), [], "{case}");
        }
    }

    /// A removed output is forgotten and its `zaura_output` released at
    /// once: a `wl_output` before v3 has no release request to free them
    /// later. The guest's object itself stays valid until it releases it.
    #[tokio::test]
    async fn global_remove_forgets_the_output_and_releases_its_aura_child() {
        let mut r = Registries::aura_placement().await;
        r.global(A, 11, zaura_shell::NAME, 38).await;
        let host = r.s.host_received();
        let [shell] = &host[..] else {
            panic!("the host got {host:?}, not the shell's bind");
        };
        let shell = bound(shell);
        r.global(A, 10, "wl_output", 2).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(11, "gtk_shell1", 1)),
                (r.a.id, global(10, "wl_output", 2)),
            ]
        );
        let output = r.bind(r.a, 10, "wl_output", 2).await;
        let host = r.s.host_received();
        let [bind, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput { id, .. }))] =
            &host[..]
        else {
            panic!("the output got no Aura output: {host:?}");
        };
        let (output_host, aura_output) = (bound(bind), *id);
        let get_aura_output = zaura_shell::Request::GetAuraOutput {
            id: aura_output,
            output: output_host,
        };
        assert_eq!(host[1], (shell, get_aura_output.into()));

        r.remove(A, 10).await;
        assert_eq!(
            r.s.host_received(),
            [(aura_output, zaura_output::Request::Release {}.into())]
        );
        assert_eq!(r.s.guest_received(), [(r.a.id, global_remove(10))]);
        // Reads state no peer can observe: placement no longer reads the
        // output.
        assert!(r.s.ctx().outputs.is_empty_for_test());

        r.s.host([wl_output::evt::done(output_host)]).await;
        assert_eq!(
            r.s.guest_received(),
            [(output, wl_output::Event::Done {}.into())],
            "the guest's output stays valid"
        );
    }

    /// A bind through a guest registry with no host object cannot be sent,
    /// so it registers no output and fails closed: the session ends. No peer
    /// can make such a registry, so this test builds the proxy's state by
    /// hand.
    #[test]
    fn a_bind_through_an_unmapped_registry_fails_closed() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let (registry, host_registry) = (GuestId::for_test(10), HostId::for_test(100));
        ctx.shadow_table
            .insert_paired_for_test(registry, host_registry, Iface::WlRegistry);
        let sender = ctx.shadow_table.resolve_event(100).expect("the registry");
        let output = "wl_output".to_string();
        assert_eq!(
            Handlers::on_global(&mut ctx, sender, 10, &output, 3),
            Action::Forward
        );

        // The guest's registry 12 has no host object, so the bind cannot be
        // sent.
        let unmapped = GuestId::for_test(12);
        ctx.shadow_table
            .create_local(unmapped, Iface::WlRegistry, 1);
        let sender = ctx.shadow_table.resolve_request(12).expect("the registry");
        let id = GuestId::for_test(20);
        Handlers::on_bind(&mut ctx, sender, 10, &(output, 3, id));
        assert!(ctx.is_fatal());
        assert_eq!(ctx.shadow_table.get_host_id(id), None);
        assert!(!ctx.sends_anything());
        assert!(ctx.outputs.is_empty_for_test());
    }

    /// A bind the proxy cannot serve is a protocol error on the guest's
    /// registry, and reaches no host.
    #[tokio::test]
    async fn an_invalid_bind_is_a_protocol_error() {
        struct Case {
            name: &'static str,
            setup: &'static [Step],
            /// The registry the guest binds through.
            through: Registry,
            bind: (u32, &'static str, u32),
        }
        for case in [
            Case {
                name: "a global of another interface",
                setup: &[Global(A, 7, "wl_compositor", 1)],
                through: A,
                bind: (7, "wl_seat", 1),
            },
            Case {
                name: "a name never advertised",
                setup: &[],
                through: A,
                bind: (42, "wl_seat", 5),
            },
            // Defence in depth: `host_globals` only ever holds forwarded
            // interfaces, so this bind fails at the name lookup as well and
            // the forwarding guard cannot be isolated by a test.
            Case {
                name: "an interface the proxy does not forward",
                setup: &[],
                through: A,
                bind: (42, "zcr_text_input_extension_v1", 1),
            },
            Case {
                name: "a global hidden without GPU access",
                setup: &[Global(A, 42, "zwp_linux_dmabuf_v1", 5)],
                through: A,
                bind: (42, "zwp_linux_dmabuf_v1", 5),
            },
            Case {
                name: "a version above the advertised one",
                setup: &[Global(A, 10, "wl_seat", 3)],
                through: A,
                bind: (10, "wl_seat", 4),
            },
            Case {
                // Only an output can be bound inert after its removal (see
                // `bind_crossing_an_output_removal_creates_an_inert_output`).
                // The removal of a name the registry never listed is
                // dropped.
                name: "a removed seat",
                setup: &[Global(A, 10, "wl_seat", 5), Remove(A, 99), Remove(A, 10)],
                through: A,
                bind: (10, "wl_seat", 5),
            },
            Case {
                // A bind that crossed an output's removal is inert only as
                // the output it was.
                name: "a removed output bound as another interface",
                setup: &[Global(A, 10, "wl_output", 3), Remove(A, 10)],
                through: A,
                bind: (10, "wl_seat", 3),
            },
            Case {
                name: "a live global the binding registry never listed",
                setup: &[Global(A, 10, "wl_output", 3), Global(B, 11, "wl_seat", 5)],
                through: B,
                bind: (10, "wl_output", 3),
            },
            Case {
                // Registry B listed nothing, so only the retirement of the
                // global its removal from A completed can refuse the bind.
                name: "a retired global bound through a registry that listed nothing",
                setup: &[Global(A, 10, "wl_seat", 5), Remove(A, 10)],
                through: B,
                bind: (10, "wl_seat", 5),
            },
        ] {
            let mut r = Registries::new().await;
            for &step in case.setup {
                r.run(step).await;
            }
            r.s.guest_received();

            let (name, interface, version) = case.bind;
            let through = r.of(case.through);
            let id = r.s.new_id();
            let bind = wl_registry::req::bind(through.id, name, interface, version, id).unwrap();
            r.s.guest_fatal([bind]).await;
            assert_eq!(
                r.s.guest_received_and_error(),
                (vec![], (through.id, wl_display::ERROR_INVALID_OBJECT)),
                "{}",
                case.name
            );
            assert_eq!(r.s.host_received(), [], "{}", case.name);
        }
    }

    /// An output bound before the proxy binds `zaura_shell` gets its Aura
    /// output when the shell is bound.
    #[tokio::test]
    async fn late_zaura_shell_binding_attaches_existing_outputs() {
        let mut r = Registries::aura_placement().await;
        r.global(A, 10, "wl_output", 3).await;
        r.s.guest_received();
        // The host gets the bind alone: the Aura output waits for the
        // shell's binding.
        let output = r.bind_paired(r.a, 10, "wl_output", 3).await;

        r.global(A, 11, zaura_shell::NAME, 38).await;
        assert_eq!(
            r.s.guest_received(),
            [(r.a.id, global(11, "gtk_shell1", 1))]
        );
        let host = r.s.host_received();
        let [bind, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput { id, .. }))] =
            &host[..]
        else {
            panic!("late shell binding must attach every already-bound output: {host:?}");
        };
        let shell = bound(bind);
        let get_aura_output = zaura_shell::Request::GetAuraOutput {
            id: *id,
            output: output.host,
        };
        assert_eq!(
            host,
            [
                (r.a.host, bind_request(11, zaura_shell::NAME, 38, shell)),
                (shell, get_aura_output.into()),
            ]
        );
    }

    /// A toplevel created before zaura_shell is bound gets its Aura
    /// toplevel when the shell is bound.
    #[tokio::test]
    async fn late_zaura_shell_binding_attaches_existing_toplevels() {
        let mut r = Registries::aura_placement().await;
        let toplevel = r.s.toplevel().await;
        assert_eq!(r.s.host_received(), []);

        r.global(A, 11, zaura_shell::NAME, 38).await;
        assert_eq!(
            r.s.guest_received(),
            [(r.a.id, global(11, "gtk_shell1", 1))]
        );
        let host = r.s.host_received();
        let [bind, (
            _,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                id, ..
            }),
        ), _] = &host[..]
        else {
            panic!("late shell binding must attach every recorded toplevel: {host:?}");
        };
        let (shell, aura_toplevel) = (bound(bind), *id);
        let get_aura_toplevel = zaura_shell::Request::GetAuraToplevelForXdgToplevel {
            id: aura_toplevel,
            toplevel: toplevel.toplevel.host,
        };
        assert_eq!(
            host,
            [
                (r.a.host, bind_request(11, zaura_shell::NAME, 38, shell)),
                (shell, get_aura_toplevel.into()),
                (
                    aura_toplevel,
                    zaura_toplevel::Request::SetSupportsScreenCoordinates {}.into()
                ),
            ]
        );
    }

    /// The guest's `wl_shm` is emulated over the proxy's one host binding.
    #[tokio::test]
    async fn synthetic_shm_bind_does_not_create_a_second_host_object() {
        let mut r = Registries::new().await;
        r.global(A, 7, wl_shm::NAME, 1).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(7, wl_shm::NAME, 1))]);
        assert_eq!(r.s.host_received().len(), 1, "the proxy binds wl_shm");

        let shm = r.bind(r.a, 7, wl_shm::NAME, 1).await;
        assert_served_locally(&mut r.s, shm, wl_shm::NAME).await;
    }

    /// Every guest `zwp_text_input_manager_v3` is local: mapping each to
    /// the one host manager would let one's destruction unmap the others'.
    #[tokio::test]
    async fn synthetic_text_input_managers_do_not_alias_shared_host_id() {
        let mut r = Registries::new().await;
        r.global(A, 9, zwp_text_input_manager_v1::NAME, 1).await;
        let extension = zcr_text_input_extension_v1::VERSION;
        r.global(A, 10, zcr_text_input_extension_v1::NAME, extension)
            .await;
        let host = r.s.host_received();
        let [manager, _] = &host[..] else {
            panic!("the host got {host:?}, not the managers' binds");
        };
        let manager = bound(manager);
        r.s.guest_received();
        let manager_v3 = zwp_text_input_manager_v3::NAME;
        let first = r.bind(r.a, 9, manager_v3, 1).await;
        let second = r.bind(r.a, 9, manager_v3, 1).await;
        assert_eq!(r.s.host_received(), [], "the managers are local");

        r.s.guest([zwp_text_input_manager_v3::req::destroy(first)])
            .await;
        assert_eq!(r.s.host_received(), []);
        let deleted = wl_display::Event::DeleteId { id: first };
        assert_eq!(r.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        // The host manager still serves the other one.
        let seat = r.s.bind("wl_seat", 5).await;
        let text_input = r.s.new_id();
        r.s.guest([zwp_text_input_manager_v3::req::get_text_input(
            second, text_input, seat,
        )])
        .await;
        let host = r.s.host_received();
        assert!(
            matches!(
                &host[..],
                [(sender, AnyRequest::ZwpTextInputManagerV1(
                    zwp_text_input_manager_v1::Request::CreateTextInput { .. }
                )), _] if *sender == manager
            ),
            "the host got {host:?}, not a text input of its manager"
        );
    }

    #[tokio::test]
    async fn duplicate_interfaces_keep_each_global_name_bindable() {
        let mut r = Registries::new().await;
        r.global(A, 10, "wl_seat", 5).await;
        r.global(A, 11, "wl_seat", 5).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(10, "wl_seat", 5)),
                (r.a.id, global(11, "wl_seat", 5)),
            ]
        );

        r.bind_paired(r.a, 10, "wl_seat", 5).await;
        r.bind_paired(r.a, 11, "wl_seat", 5).await;
    }

    #[tokio::test]
    async fn removed_text_input_globals_keep_bound_manager_usable() {
        let mut r = Registries::new().await;
        r.global(A, 9, zwp_text_input_manager_v1::NAME, 1).await;
        r.global(A, 10, zcr_text_input_extension_v1::NAME, 11).await;
        let host = r.s.host_received();
        let [manager, extension] = &host[..] else {
            panic!("the host got {host:?}, not the managers' binds");
        };
        let (manager, extension) = (bound(manager), bound(extension));
        r.s.guest_received();

        // The synthetic v3 global is advertised under the host manager's
        // numeric name. Bind a guest manager before removing the host globals.
        let manager_v3 = r.bind(r.a, 9, zwp_text_input_manager_v3::NAME, 1).await;
        r.remove(A, 9).await;
        r.remove(A, 10).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global_remove(9))]);
        assert_eq!(r.s.host_received(), []);

        // An already-bound manager remains usable after global_remove. New
        // text-input children must still be created through both retained
        // host-side factories.
        let seat = r.s.bind("wl_seat", 5).await;
        let text_input = r.s.new_id();
        r.s.guest([zwp_text_input_manager_v3::req::get_text_input(
            manager_v3, text_input, seat,
        )])
        .await;
        let host = r.s.host_received();
        // The host IDs come from the requests that create them, and the
        // extension extends the text input the manager creates.
        let created = |request: &(u32, AnyRequest)| request.1.new_object().map(|(id, _)| id);
        let (Some(host_text_input), Some(host_extended)) = (created(&host[0]), created(&host[1]))
        else {
            panic!("the host got no extended text input: {host:?}");
        };
        assert_eq!(
            host,
            [
                (
                    manager,
                    zwp_text_input_manager_v1::Request::CreateTextInput {
                        id: host_text_input
                    }
                    .into()
                ),
                (
                    extension,
                    zcr_text_input_extension_v1::Request::GetExtendedTextInput {
                        id: host_extended,
                        text_input: host_text_input,
                    }
                    .into()
                ),
            ]
        );
    }

    /// The `zcr_extended_keyboard_v1` of a keyboard outlives its manager's
    /// global: the keyboard's release still destroys it.
    #[tokio::test]
    async fn keyboard_extension_global_remove_keeps_existing_children_alive() {
        let mut r = Registries::new().await;
        r.global(A, 20, zcr_keyboard_extension_v1::NAME, 2).await;
        let host = r.s.host_received();
        let [extension] = &host[..] else {
            panic!("the host got {host:?}, not the extension's bind");
        };
        let extension = bound(extension);
        let (_, keyboard) = r.s.keyboard().await;
        let surface = r.s.surface().await;
        r.s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
            .await;
        assert_eq!(r.s.guest_received().len(), 2, "the enter and its modifiers");
        let host = r.s.host_received();
        let [(
            sender,
            AnyRequest::ZcrKeyboardExtensionV1(
                zcr_keyboard_extension_v1::Request::GetExtendedKeyboard {
                    id: child,
                    keyboard: of,
                },
            ),
        )] = host[..]
        else {
            panic!("the keyboard got no extended keyboard: {host:?}");
        };
        assert_eq!((sender, of), (extension, keyboard.host));

        r.remove(A, 20).await;
        assert_eq!(r.s.host_received(), []);
        assert_eq!(r.s.guest_received(), []);

        r.s.guest([wl_keyboard::req::release(keyboard.id)]).await;
        assert_eq!(
            r.s.host_received(),
            [
                (keyboard.host, wl_keyboard::Request::Release {}.into()),
                (child, zcr_extended_keyboard_v1::Request::Destroy {}.into()),
            ],
            "an existing child must remain mapped after manager removal"
        );
    }

    /// Removing the global a singleton is bound to retires the binding: its
    /// host ID stays reserved, the host object is destroyed if its
    /// interface has a destructor, and the next global of its interface is
    /// bound afresh.
    #[tokio::test]
    async fn removing_a_bound_singleton_global_retires_its_binding() {
        struct Case {
            singleton: Singleton,
            /// The version the host advertises, and the one the proxy binds.
            version: u32,
            bound_version: u32,
            /// The global the guest is advertised for it, which its removal
            /// reaches.
            advertised: Option<(&'static str, u32)>,
            /// The request that destroys the old binding, if its interface
            /// has one.
            destructor: Option<AnyRequest>,
            /// Whether a new binding asks for its capabilities with a sync.
            capability_sync: bool,
        }
        for Case {
            singleton,
            version,
            bound_version,
            advertised,
            destructor,
            capability_sync,
        } in [
            Case {
                singleton: Singleton::Shm,
                version: 1,
                bound_version: 1,
                advertised: Some((wl_shm::NAME, 1)),
                destructor: None,
                capability_sync: false,
            },
            Case {
                singleton: Singleton::KeyboardExtension,
                version: 2,
                bound_version: 2,
                advertised: None,
                destructor: None,
                capability_sync: false,
            },
            Case {
                singleton: Singleton::AuraShell,
                version: 38,
                bound_version: 38,
                advertised: Some((gtk_shell1::NAME, 1)),
                destructor: Some(zaura_shell::Request::Release {}.into()),
                capability_sync: false,
            },
            // Removed before its capabilities arrived, the global was never
            // shown to the guest.
            Case {
                singleton: Singleton::Dmabuf,
                version: 4,
                bound_version: 3,
                advertised: None,
                destructor: Some(zwp_linux_dmabuf_v1::Request::Destroy {}.into()),
                capability_sync: true,
            },
        ] {
            let interface = singleton.interface().name();
            let mut r = Registries::gpu(true).await;
            let a = r.a;
            let advertise = |name| {
                Vec::from_iter(
                    advertised
                        .map(|(advertised, version)| (a.id, global(name, advertised, version))),
                )
            };
            // The proxy binds global `name` as `version`, and asks for its
            // capabilities if it has some; returns the binding.
            let binding = |name, host: Vec<(u32, AnyRequest)>| {
                let binding = bound(&host[0]);
                let expected = bind_request(name, interface, bound_version, binding);
                let sync = capability_sync.then(|| {
                    let callback = host[1].1.new_object().expect("a callback").0;
                    sync(callback)
                });
                assert_eq!(
                    host,
                    Vec::from_iter([(a.host, expected)].into_iter().chain(sync)),
                    "{interface}"
                );
                binding
            };
            r.global(A, 10, interface, version).await;
            let old = binding(10, r.s.host_received());
            assert_eq!(r.s.guest_received(), advertise(10), "{interface}");

            r.remove(A, 10).await;
            assert_eq!(
                r.s.host_received(),
                Vec::from_iter(destructor.map(|request| (old, request))),
                "{interface}"
            );
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(advertised.map(|_| (a.id, global_remove(10)))),
                "{interface}"
            );
            // Reads state no peer can observe: the guest global that waited
            // for the binding's capabilities, which no registry lists now.
            assert!(r.s.ctx().globals.pending_dmabuf.is_empty(), "{interface}");

            r.global(A, 11, interface, version).await;
            let new = binding(11, r.s.host_received());
            assert_ne!(new, old, "{interface}");
            assert_eq!(r.s.guest_received(), advertise(11), "{interface}");
        }
    }

    /// Removing the `zaura_shell` global releases the proxy's binding if
    /// its version has a destructor, and keeps the `zaura_surface`s made
    /// from it until their surfaces are destroyed.
    #[tokio::test]
    async fn global_remove_keeps_aura_children_until_surface_destroy() {
        for (version, released) in [(38, true), (37, false)] {
            let mut r = Registries::new().await;
            r.global(A, 10, zaura_shell::NAME, version).await;
            let host = r.s.host_received();
            let [shell] = &host[..] else {
                panic!("the host got {host:?}, not the shell's bind");
            };
            let shell = bound(shell);
            r.s.guest_received();
            let gtk_shell = r.bind(r.a, 10, gtk_shell1::NAME, 1).await;
            assert_served_locally(&mut r.s, gtk_shell, gtk_shell1::NAME).await;
            let surface = r.s.surface().await;
            let gtk_surface = r.s.new_id();
            r.s.guest([
                gtk_shell1::req::set_startup_id(gtk_shell, Some("launch")).unwrap(),
                gtk_shell1::req::get_gtk_surface(gtk_shell, gtk_surface, surface.id),
            ])
            .await;
            let host = r.s.host_received();
            let [(_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), _] =
                host[..]
            else {
                panic!("the surface got no Aura surface: {host:?}");
            };
            let aura_surface = id;
            let get_aura_surface = zaura_shell::Request::GetAuraSurface {
                id: aura_surface,
                surface: surface.host,
            };
            let startup_id = zaura_surface::Request::SetStartupId {
                startup_id: Some("launch".to_string()),
            };
            assert_eq!(
                host,
                [
                    (shell, get_aura_surface.into()),
                    (aura_surface, startup_id.into())
                ]
            );

            r.remove(A, 10).await;
            assert_eq!(
                r.s.host_received(),
                Vec::from_iter(released.then(|| (shell, zaura_shell::Request::Release {}.into()))),
                "v{version}"
            );
            assert_eq!(r.s.guest_received(), [(r.a.id, global_remove(10))]);

            // The Aura surface outlives the global: GTK still names the
            // application on it.
            let application = Some("com.example.Terminal");
            r.s.guest([gtk_surface1::req::set_dbus_properties(
                gtk_surface,
                application,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap()])
                .await;
            let host = r.s.host_received();
            assert!(
                matches!(
                    &host[..],
                    [(sender, AnyRequest::ZauraSurface(
                        zaura_surface::Request::SetApplicationId { .. }
                    ))] if *sender == aura_surface
                ),
                "the host got {host:?}, not the application of the Aura surface"
            );

            // Until its surface is destroyed: an Aura surface of v38
            // releases with it.
            r.s.guest([wl_surface::req::destroy(surface.id)]).await;
            let release =
                (released).then(|| (aura_surface, zaura_surface::Request::Release {}.into()));
            assert_eq!(
                r.s.host_received(),
                Vec::from_iter(
                    release
                        .into_iter()
                        .chain([(surface.host, wl_surface::Request::Destroy {}.into())])
                ),
                "v{version}"
            );
            assert_eq!(r.s.guest_received(), []);
        }
    }

    /// A host may advertise a singleton global under a new name before it
    /// removes the old one. The proxy keeps its binding of the old name,
    /// and binds the new one when the old one retires.
    #[tokio::test]
    async fn a_singleton_advertised_anew_is_bound_when_the_old_name_retires() {
        struct Case {
            singleton: Singleton,
            /// The version the host advertises both globals at, and the
            /// one the proxy binds.
            version: u32,
            bound_version: u32,
            /// The global the guest is advertised for each, which its
            /// removal reaches.
            advertised: Option<(&'static str, u32)>,
            /// The request that destroys the old binding, if its interface
            /// has one.
            destructor: Option<AnyRequest>,
        }
        for Case {
            singleton,
            version,
            bound_version,
            advertised,
            destructor,
        } in [
            Case {
                singleton: Singleton::Dmabuf,
                version: LINUX_DMABUF_CAPABILITY_VERSION,
                bound_version: 3,
                advertised: Some((zwp_linux_dmabuf_v1::NAME, 3)),
                destructor: Some(zwp_linux_dmabuf_v1::Request::Destroy {}.into()),
            },
            Case {
                singleton: Singleton::KeyboardExtension,
                version: zcr_keyboard_extension_v1::VERSION,
                bound_version: 2,
                advertised: None,
                destructor: None,
            },
            // Before v38 `zaura_shell` has no destructor.
            Case {
                singleton: Singleton::AuraShell,
                version: ZAURA_SHELL_MIN_VERSION,
                bound_version: 6,
                advertised: Some((gtk_shell1::NAME, 1)),
                destructor: None,
            },
            Case {
                singleton: Singleton::Shm,
                version: WL_SHM_VERSION,
                bound_version: 1,
                advertised: Some((wl_shm::NAME, 1)),
                destructor: None,
            },
        ] {
            let mut r = Registries::gpu(false).await;
            let interface = singleton.interface().name();
            let a = r.a;
            let advertise = |name| {
                Vec::from_iter(
                    advertised
                        .map(|(advertised, version)| (a.id, global(name, advertised, version))),
                )
            };
            r.global(A, 10, interface, version).await;
            assert_eq!(r.s.guest_received(), advertise(10), "{interface}");
            let host = r.s.host_received();
            let [old] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let old = bound(old);
            assert_eq!(
                host,
                [(a.host, bind_request(10, interface, bound_version, old))],
                "{interface} is bound"
            );
            r.global(A, 11, interface, version).await;
            assert_eq!(r.s.guest_received(), advertise(11), "{interface}");
            assert_eq!(r.s.host_received(), [], "{interface} is bound once");

            r.remove(A, 10).await;

            let host = r.s.host_received();
            let new = bound(host.last().expect("a new binding"));
            let expected = destructor.map(|request| (old, request)).into_iter();
            assert_eq!(
                host,
                Vec::from_iter(
                    expected.chain([(a.host, bind_request(11, interface, bound_version, new))])
                ),
                "{interface}"
            );
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(advertised.map(|_| (a.id, global_remove(10)))),
                "{interface}"
            );
        }
    }

    /// When the proxy's `wl_shm` binding retires, it binds the lowest name
    /// that a registry still lists and whose removal no registry has seen,
    /// through the lowest registry that lists it.
    #[tokio::test]
    async fn a_singleton_binding_moves_to_the_lowest_live_candidate() {
        struct Case {
            name: &'static str,
            /// The `wl_shm` names each registry lists, in the order the
            /// host advertised them. The proxy binds the first.
            listings: &'static [(Registry, &'static [u32])],
            steps: &'static [Step],
            /// The name the proxy has bound after the steps.
            bound: Option<u32>,
            /// The `wl_shm` binds the steps sent, through each registry, of
            /// each name.
            binds: &'static [(Registry, u32)],
            candidates: &'static [u32],
        }
        for case in [
            Case {
                name: "the lowest candidate is bound through the lowest registry",
                listings: &[(B, &[10, 12, 11]), (A, &[10, 12, 11])],
                steps: &[Remove(B, 10), Remove(A, 10)],
                bound: Some(11),
                binds: &[(A, 11)],
                candidates: &[11, 12],
            },
            Case {
                // A compositor may destroy a global as soon as it removes
                // it, so a bind of a name one registry saw removed could
                // end the connection.
                name: "a replacement another registry saw removed is not bound",
                listings: &[(A, &[10, 11]), (B, &[10, 11])],
                steps: &[Remove(A, 10), Remove(A, 11), Remove(B, 10)],
                bound: None,
                binds: &[],
                candidates: &[11],
            },
            Case {
                name: "a replacement every registry saw removed is not bound",
                listings: &[(A, &[10, 11]), (B, &[10, 11])],
                steps: &[Remove(A, 10), Remove(A, 11), Remove(B, 10), Remove(B, 11)],
                bound: None,
                binds: &[],
                candidates: &[],
            },
            Case {
                name: "a replacement removed first is not bound",
                listings: &[(A, &[10, 11])],
                steps: &[Remove(A, 11), Remove(A, 10)],
                bound: None,
                binds: &[],
                candidates: &[],
            },
            Case {
                name: "a binding another registry lists stays",
                listings: &[(A, &[10, 11]), (B, &[10, 12])],
                steps: &[Remove(A, 10)],
                bound: Some(10),
                binds: &[],
                candidates: &[10, 11, 12],
            },
            Case {
                // Destroying the last registry to list the removed binding
                // retires it; a candidate only that registry listed is
                // forgotten.
                name: "destroying a registry can promote a replacement",
                listings: &[(A, &[10, 11]), (B, &[10, 12])],
                steps: &[Remove(A, 10), Destroy(B)],
                bound: Some(11),
                binds: &[(A, 11)],
                candidates: &[11],
            },
        ] {
            let mut r = Registries::new().await;
            let mut host = Vec::new();
            let listings = case.listings.iter().flat_map(|&(registry, names)| {
                names
                    .iter()
                    .map(move |&name| Global(registry, name, "wl_shm", 1))
            });
            for step in listings.chain(case.steps.iter().copied()) {
                r.s.guest_received();
                host.extend(r.run(step).await);
            }
            r.s.guest_received();

            // The first name listed is bound through its registry, then
            // each the steps bind.
            let (first, names) = case.listings[0];
            let binds = [(first, names[0])]
                .into_iter()
                .chain(case.binds.iter().copied());
            let expected = binds.zip(&host).map(|((registry, name), request)| {
                let bind = bind_request(name, "wl_shm", 1, bound(request));
                (r.of(registry).host, bind)
            });
            assert_eq!(host, Vec::from_iter(expected), "{}", case.name);
            // Reads state no peer can observe: the names a later
            // retirement could bind.
            let mut candidates = Vec::from_iter(r.s.ctx().globals.singleton_candidates_for_test());
            candidates.sort_unstable();
            assert_eq!(candidates, case.candidates, "{}", case.name);

            // A new name is bound at once only if no other is.
            r.global(A, 99, "wl_shm", 1).await;
            r.s.guest_received();
            let host = r.s.host_received();
            assert_eq!(host.is_empty(), case.bound.is_some(), "{}", case.name);
            if case.bound.is_none() {
                let expected = bind_request(99, "wl_shm", 1, bound(&host[0]));
                assert_eq!(host, [(r.a.host, expected)], "{}", case.name);
            }
            if let Some(bound) = case.bound {
                let last = case.binds.last().map_or(names[0], |&(_, name)| name);
                assert_eq!(last, bound, "{}", case.name);
            }
        }
    }

    /// The guest global of a replacement linux-dmabuf waits, like any, for
    /// the capabilities of its own binding.
    #[tokio::test]
    async fn a_promoted_dmabuf_publishes_its_deferred_guest_global() {
        let mut r = Registries::gpu(true).await;
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        r.global(A, 10, dmabuf, 4).await;
        let (old, _) = dmabuf_binding(&r.s.host_received(), r.a, 10);
        r.global(A, 11, dmabuf, 4).await;
        assert_eq!(r.s.host_received(), []);

        r.remove(A, 10).await;
        let host = r.s.host_received();
        let destroy = zwp_linux_dmabuf_v1::Request::Destroy {};
        assert_eq!(host[0], (old, destroy.into()));
        let (new, callback) = dmabuf_binding(&host[1..], r.a, 11);
        assert_eq!(r.s.guest_received(), []);

        r.s.host(capabilities(new, callback, &[XRGB8888_LINEAR]))
            .await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(11, dmabuf, 4))]);
    }

    /// libwayland-server lets a bind cross its global's removal on the
    /// wire, as when a dock flaps and the client binds the output it saw
    /// advertised in the same dispatch. Such an output becomes an inert
    /// guest object, of the version bound, whose release completes in the
    /// proxy. A name never advertised to the binding registry stays a
    /// protocol error.
    #[tokio::test]
    async fn bind_crossing_an_output_removal_creates_an_inert_output() {
        let mut r = Registries::new().await;
        r.global(A, 10, "wl_output", 3).await;
        r.remove(A, 10).await;
        r.global(B, 11, "wl_seat", 5).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(10, "wl_output", 3)),
                (r.a.id, global_remove(10)),
                (r.b.id, global(11, "wl_seat", 5)),
            ]
        );

        let output = r.bind(r.a, 10, "wl_output", 3).await;
        assert_eq!(r.s.host_received(), [], "nothing reaches the host");
        r.s.guest([wl_output::req::release(output)]).await;
        assert_eq!(r.s.host_received(), []);
        let deleted = wl_display::Event::DeleteId { id: output };
        assert_eq!(r.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        // Name 10 is retired, and registry B never saw it.
        r.bind_fatal(r.b, 10, "wl_output", 3).await;
    }

    /// Registries A and B list global 7, a `wl_seat`, whose removal only
    /// registry A has seen.
    async fn a_global_removed_from_one_of_two_registries() -> Registries {
        let mut r = Registries::new().await;
        for registry in [A, B] {
            r.global(registry, 7, "wl_seat", 5).await;
        }
        r.remove(A, 7).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(7, "wl_seat", 5)),
                (r.b.id, global(7, "wl_seat", 5)),
                (r.a.id, global_remove(7)),
            ]
        );
        r
    }

    /// O3: a global removed from one registry waits for every registry that
    /// still lists it. Destroying the last of those through `wl_fixes`
    /// means it never sees the removal, so the global is retired then: a
    /// registry that listed nothing can no longer bind it.
    #[tokio::test]
    async fn destroying_the_last_registry_to_list_a_removed_global_retires_it() {
        let mut r = a_global_removed_from_one_of_two_registries().await;

        assert_eq!(r.destroy(B).await, []);

        let c = r.s.new_registry().await;
        r.bind_fatal(c, 7, "wl_seat", 5).await;
    }

    /// The removal stays on record when the only registry that saw it is
    /// destroyed first, so destroying the last one still retires the
    /// global.
    #[tokio::test]
    async fn a_removal_outlives_the_registry_that_saw_it() {
        let mut r = a_global_removed_from_one_of_two_registries().await;

        assert_eq!(r.destroy(A).await, []);
        // Registry B lists it, so its bind still reaches the host.
        r.bind_paired(r.b, 7, "wl_seat", 5).await;
        assert_eq!(r.destroy(B).await, []);

        let c = r.s.new_registry().await;
        r.bind_fatal(c, 7, "wl_seat", 5).await;
    }

    /// A registry destroyed while a global is still advertised leaves it
    /// to the other registries.
    #[tokio::test]
    async fn destroying_a_registry_keeps_a_global_no_registry_saw_removed() {
        let mut r = Registries::new().await;
        r.global(A, 7, "wl_seat", 5).await;
        r.s.guest_received();

        assert_eq!(r.destroy(A).await, []);

        // Registry B, which has listed nothing yet, may still bind it.
        r.bind_paired(r.b, 7, "wl_seat", 5).await;
    }

    #[tokio::test]
    async fn compositor_guest_version_and_host_bind_are_negotiated_separately() {
        let compositor = "wl_compositor";
        // A global the guest may take as the host offers it is forwarded;
        // the proxy advertises any other at the guest's version itself.
        for (host_version, guest_version, bound_version) in
            [(1, 1, 1), (2, 2, 2), (3, 4, 3), (4, 4, 3), (6, 4, 3)]
        {
            let mut r = Registries::new().await;
            r.global(A, 7, compositor, host_version).await;
            assert_eq!(
                r.s.guest_received(),
                [(r.a.id, global(7, compositor, guest_version))],
                "v{host_version}"
            );

            r.bind(r.a, 7, compositor, guest_version).await;
            let host = r.s.host_received();
            let [binding] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let binding = bound(binding);
            assert_eq!(
                host,
                [(
                    r.a.host,
                    bind_request(7, compositor, bound_version, binding)
                )],
                "v{host_version}"
            );

            r.bind_fatal(r.a, 7, compositor, guest_version + 1).await;
        }
    }

    /// The version the guest is offered of a forwarded host `interface`
    /// at `host_version`.
    fn offered(interface: &str, host_version: u32) -> u32 {
        let iface = Iface::from_name(interface).expect("a vendored interface");
        let Policy::Forward { ceiling, serve, .. } = policy(iface) else {
            panic!("{interface} is not forwarded");
        };
        offered_version(ceiling, serve, host_version)
    }

    #[tokio::test]
    async fn ordinary_globals_are_capped_to_supported_versions() {
        for (interface, host_version, offered) in [
            ("wl_compositor", 6, 4),
            ("wl_compositor", 3, 4),
            ("wl_compositor", 2, 2),
            ("wl_output", 4, 3),
            ("wl_seat", 10, 5),
            ("wl_data_device_manager", 99, 3),
            ("xdg_wm_base", 7, 3),
            ("zxdg_output_manager_v1", 4, 3),
        ] {
            assert_eq!(
                super::tests::offered(interface, host_version),
                offered,
                "{interface} v{host_version}"
            );
        }

        let mut r = Registries::new().await;
        r.global(A, 9, "zxdg_output_manager_v1", 3).await;
        r.global(A, 10, "wl_seat", 10).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(9, "zxdg_output_manager_v1", 3)),
                (r.a.id, global(10, "wl_seat", 5)),
            ],
            "the guest must receive the capped version"
        );
        r.bind_paired(r.a, 10, "wl_seat", 5).await;
    }

    /// A guest's `wl_display.sync` against a registry whose linux-dmabuf
    /// global waits for its capabilities, played against a host that
    /// answers every request in order, as libwayland-server does.
    mod sync_after_get_registry {
        use super::*;
        use crate::protocols::wayland::wl_seat;

        /// The forwarded global every host here advertises first, name 1.
        const SEAT: (&str, u32) = (wl_seat::NAME, 5);
        /// The host's linux-dmabuf, name 2, of a version the proxy offers
        /// the guest with synthetic feedback.
        const DMABUF: (&str, u32) = (zwp_linux_dmabuf_v1::NAME, 4);

        /// A host compositor that answers each request in order: a
        /// `get_registry` with its globals, a sync with its `done` and
        /// `delete_id`, and a linux-dmabuf bind with `formats`.
        struct InOrderHost {
            globals: Vec<(&'static str, u32)>,
            formats: Vec<(u32, u64)>,
        }

        impl InOrderHost {
            /// A host that advertises [`SEAT`] and [`DMABUF`], whose
            /// linux-dmabuf offers `formats`.
            fn with_dmabuf(formats: &[(u32, u64)]) -> Self {
                Self {
                    globals: vec![SEAT, DMABUF],
                    formats: formats.to_vec(),
                }
            }

            fn answer(&self, requests: &[(u32, AnyRequest)]) -> Vec<Outgoing<u32>> {
                let mut events = Vec::new();
                for (_, request) in requests {
                    match request {
                        AnyRequest::WlDisplay(wl_display::Request::GetRegistry { registry }) => {
                            for (name, &(interface, version)) in (1..).zip(&self.globals) {
                                let global =
                                    wl_registry::evt::global(*registry, name, interface, version);
                                events.push(global.unwrap());
                            }
                        }
                        AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) => {
                            events.push(wl_callback::evt::done(*callback, 0));
                            events.push(wl_display::evt::delete_id(WL_DISPLAY_ID, *callback));
                        }
                        AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })
                            if id.0 == zwp_linux_dmabuf_v1::NAME =>
                        {
                            for &(format, modifier) in &self.formats {
                                let (hi, lo) = ((modifier >> 32) as u32, modifier as u32);
                                events
                                    .push(zwp_linux_dmabuf_v1::evt::modifier(id.2, format, hi, lo));
                            }
                        }
                        _ => {}
                    }
                }
                events
            }

            /// Answer what the proxy sends until it sends nothing more;
            /// returns every request the host got, in order.
            async fn serve(&self, s: &mut Session) -> Vec<(u32, AnyRequest)> {
                let mut all = Vec::new();
                loop {
                    let requests = s.host_received();
                    if requests.is_empty() {
                        return all;
                    }
                    let events = self.answer(&requests);
                    all.extend(requests);
                    if !events.is_empty() {
                        s.host(events).await;
                    }
                }
            }
        }

        /// A session whose proxy has GPU access and synthesizes dma-buf
        /// feedback, so it withholds the guest's linux-dmabuf global until
        /// its capabilities arrive.
        fn gpu_session() -> Session {
            let mut ctx = Context::new_for_test(true, false, vec![]);
            // Stands in for a virtio-gpu render node, which the proxy
            // synthesizes feedback for and a test cannot have.
            ctx.dmabuf.set_feedback_available_for_test(true);
            Session::with_context(ctx)
        }

        /// What the guest gets of callback `callback`: its `done`, then its
        /// `delete_id`.
        fn done(callback: u32) -> [(u32, AnyEvent); 2] {
            [
                (
                    callback,
                    wl_callback::Event::Done { callback_data: 0 }.into(),
                ),
                (
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: callback }.into(),
                ),
            ]
        }

        /// The globals registry `registry` gets of [`InOrderHost::with_dmabuf`],
        /// the seat first.
        fn seat(registry: u32) -> (u32, AnyEvent) {
            (registry, global(1, SEAT.0, SEAT.1))
        }

        fn dmabuf(registry: u32) -> (u32, AnyEvent) {
            (
                registry,
                global(
                    2,
                    DMABUF.0,
                    crate::handler::linux_dmabuf::GUEST_DMABUF_VERSION,
                ),
            )
        }

        /// Mesa's `wl_display_roundtrip` after `get_registry`: the guest
        /// gets the linux-dmabuf global before the `done`, although the host
        /// answers the sync before the proxy has the global's capabilities.
        #[tokio::test]
        async fn a_registrys_dmabuf_global_precedes_the_done_of_a_later_sync() {
            let mut s = gpu_session();
            let (registry, callback) = (s.new_id(), s.new_id());
            s.guest([
                wl_display::req::get_registry(WL_DISPLAY_ID, registry),
                wl_display::req::sync(WL_DISPLAY_ID, callback),
            ])
            .await;
            InOrderHost::with_dmabuf(&[XRGB8888_LINEAR])
                .serve(&mut s)
                .await;
            let mut expected = vec![seat(registry), dmabuf(registry)];
            expected.extend(done(callback));
            assert_eq!(s.guest_received(), expected);
        }

        /// A linux-dmabuf global the proxy hides, as the host offers no
        /// format it supports, is decided before the `done` too: the
        /// guest's sync reaches the host only after the capabilities.
        #[tokio::test]
        async fn a_hidden_dmabuf_global_is_decided_before_the_done_of_a_later_sync() {
            let mut s = gpu_session();
            let (registry, callback) = (s.new_id(), s.new_id());
            s.guest([
                wl_display::req::get_registry(WL_DISPLAY_ID, registry),
                wl_display::req::sync(WL_DISPLAY_ID, callback),
            ])
            .await;
            let host = InOrderHost::with_dmabuf(&[]).serve(&mut s).await;
            let mut expected = vec![seat(registry)];
            expected.extend(done(callback));
            assert_eq!(s.guest_received(), expected);
            let syncs: Vec<usize> = (0..host.len())
                .filter(|&at| {
                    matches!(
                        host[at].1,
                        AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
                    )
                })
                .collect();
            let capabilities = host
                .iter()
                .position(|request| matches!(request.1, AnyRequest::WlRegistry(_)))
                .expect("the proxy binds the host's linux-dmabuf")
                + 1;
            assert!(
                syncs.last() > Some(&capabilities),
                "the guest's sync follows the capability sync: {host:?}"
            );
        }

        /// Every registry the guest created before its sync gets its
        /// linux-dmabuf global before the `done`.
        #[tokio::test]
        async fn every_earlier_registry_gets_its_dmabuf_global_before_the_done() {
            let mut s = gpu_session();
            let (first, second, callback) = (s.new_id(), s.new_id(), s.new_id());
            s.guest([
                wl_display::req::get_registry(WL_DISPLAY_ID, first),
                wl_display::req::get_registry(WL_DISPLAY_ID, second),
                wl_display::req::sync(WL_DISPLAY_ID, callback),
            ])
            .await;
            InOrderHost::with_dmabuf(&[XRGB8888_LINEAR])
                .serve(&mut s)
                .await;
            let mut expected = vec![seat(first), seat(second), dmabuf(first), dmabuf(second)];
            expected.extend(done(callback));
            assert_eq!(s.guest_received(), expected);
        }

        /// A sync sent before `get_registry` follows no global of the
        /// registry, so its `done` is not held back.
        #[tokio::test]
        async fn a_sync_before_get_registry_is_answered_at_once() {
            let mut s = gpu_session();
            let (early, registry, late) = (s.new_id(), s.new_id(), s.new_id());
            s.guest([
                wl_display::req::sync(WL_DISPLAY_ID, early),
                wl_display::req::get_registry(WL_DISPLAY_ID, registry),
                wl_display::req::sync(WL_DISPLAY_ID, late),
            ])
            .await;
            InOrderHost::with_dmabuf(&[XRGB8888_LINEAR])
                .serve(&mut s)
                .await;
            let mut expected = Vec::from(done(early));
            expected.extend([seat(registry), dmabuf(registry)]);
            expected.extend(done(late));
            assert_eq!(s.guest_received(), expected);
        }

        /// The events of a request the guest sends after the sync follow
        /// its `done`: here a second registry's globals.
        #[tokio::test]
        async fn a_later_requests_events_follow_the_done() {
            let mut s = gpu_session();
            let (first, callback, second) = (s.new_id(), s.new_id(), s.new_id());
            s.guest([
                wl_display::req::get_registry(WL_DISPLAY_ID, first),
                wl_display::req::sync(WL_DISPLAY_ID, callback),
                wl_display::req::get_registry(WL_DISPLAY_ID, second),
            ])
            .await;
            InOrderHost::with_dmabuf(&[XRGB8888_LINEAR])
                .serve(&mut s)
                .await;
            let mut expected = vec![seat(first), dmabuf(first)];
            expected.extend(done(callback));
            expected.extend([seat(second), dmabuf(second)]);
            assert_eq!(s.guest_received(), expected);
        }

        /// Without synthetic dma-buf feedback the proxy withholds no
        /// global, so the host gets the guest's `get_registry` and sync as
        /// they are, and nothing else.
        #[tokio::test]
        async fn a_proxy_that_withholds_no_global_forwards_the_sync_at_once() {
            // No GPU access, then GPU access without a render node.
            for gpu_accel in [false, true] {
                let mut s = Session::with_context(Context::new_for_test(gpu_accel, false, vec![]));
                let (registry, callback) = (s.new_id(), s.new_id());
                s.guest([
                    wl_display::req::get_registry(WL_DISPLAY_ID, registry),
                    wl_display::req::sync(WL_DISPLAY_ID, callback),
                ])
                .await;
                let host = s.host_received();
                let [(WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::GetRegistry { .. })), (
                    WL_DISPLAY_ID,
                    AnyRequest::WlDisplay(wl_display::Request::Sync {
                        callback: host_callback,
                    }),
                )] = host[..]
                else {
                    panic!("the host got {host:?}, not the guest's two requests");
                };
                s.host([
                    wl_callback::evt::done(host_callback, 0),
                    wl_display::evt::delete_id(WL_DISPLAY_ID, host_callback),
                ])
                .await;
                assert_eq!(s.guest_received(), done(callback), "gpu_accel {gpu_accel}");
            }
        }
    }
}
