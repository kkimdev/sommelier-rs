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

//! The host globals the proxy binds once per connection, for its own use,
//! and its binding of each. The policy that binds, moves and releases
//! them is `handler::registry`'s (`policy`).

use std::collections::BTreeMap;

use super::HostId;
use crate::protocols::Iface;

/// A host global the proxy binds once per connection, for its own use.
///
/// Every advertisement of one is a candidate
/// (`handler::registry::Globals`), and the registry binds one whenever
/// the proxy has none. A host may advertise one under a new name before it
/// removes the old one; the new name is then bound when the old one
/// retires, so the proxy keeps the capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Singleton {
    Dmabuf,
    KeyboardExtension,
    AuraShell,
    Shm,
}

impl Singleton {
    /// Every singleton, in the order the proxy binds them when several
    /// have no binding (test: `every_interface_has_one_global_policy`).
    pub(crate) const ALL: [Self; 4] = [
        Self::Dmabuf,
        Self::KeyboardExtension,
        Self::AuraShell,
        Self::Shm,
    ];

    /// The interface of the global it binds.
    pub(crate) const fn interface(self) -> Iface {
        match self {
            Self::Dmabuf => Iface::ZwpLinuxDmabufV1,
            Self::KeyboardExtension => Iface::ZcrKeyboardExtensionV1,
            Self::AuraShell => Iface::ZauraShell,
            Self::Shm => Iface::WlShm,
        }
    }
}

/// The proxy's binding of a [`Singleton`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    /// The host object.
    pub(crate) host: HostId,
    /// The registry name of the global it was bound from, so that the
    /// global's `global_remove` releases exactly this binding.
    pub(crate) name: u32,
    /// The bound version. Every child created from the binding (an Aura
    /// surface, an extended keyboard) takes it.
    pub(crate) version: u32,
}

/// The proxy's binding of each [`Singleton`] it has bound.
#[derive(Debug, Default)]
pub(crate) struct Singletons(BTreeMap<Singleton, Binding>);

impl Singletons {
    /// The binding of `singleton`, if the proxy has one.
    pub(crate) fn get(&self, singleton: Singleton) -> Option<Binding> {
        self.0.get(&singleton).copied()
    }

    /// The host object of `singleton`'s binding, if the proxy has one.
    pub(crate) fn host(&self, singleton: Singleton) -> Option<HostId> {
        Some(self.get(singleton)?.host)
    }

    /// Record `binding` as the one of `singleton`, which has none.
    pub(crate) fn bind(&mut self, singleton: Singleton, binding: Binding) {
        let previous = self.0.insert(singleton, binding);
        debug_assert!(previous.is_none(), "{singleton:?} is bound twice");
    }

    /// End the binding that global `name` backs, if one does, and return
    /// it with its singleton.
    pub(crate) fn release_named(&mut self, name: u32) -> Option<(Singleton, Binding)> {
        let (&singleton, _) = self.0.iter().find(|(_, binding)| binding.name == name)?;
        Some((singleton, self.0.remove(&singleton)?))
    }

    /// Bind `singleton` as host object `host`, from a global named as it is,
    /// at its newest version, as the registry would.
    #[cfg(test)]
    pub(crate) fn bind_for_test(&mut self, singleton: Singleton, host: HostId) {
        let version = singleton.interface().version();
        self.0.insert(
            singleton,
            Binding {
                host,
                name: crate::wire::WireId::wire(host),
                version,
            },
        );
    }

    /// Forget `singleton`'s binding, for a test of a proxy that has none.
    #[cfg(test)]
    pub(crate) fn unbind_for_test(&mut self, singleton: Singleton) {
        self.0.remove(&singleton);
    }
}
