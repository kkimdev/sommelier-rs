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
//! them is `handler::registry`'s: each singleton's row in its table
//! (`policy`) names the interface it binds.

use std::collections::BTreeMap;

use super::HostId;

/// A host global the proxy binds once per connection, for its own use.
///
/// Every advertisement of one is a candidate
/// (`handler::registry::Globals`), and the registry binds one whenever
/// the proxy has none. A host may advertise one under a new name before it
/// removes the old one; the new name is then bound when the old one
/// retires, so the proxy keeps the capability. The text-input managers
/// are the exception: the proxy keeps the first it binds for the
/// connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Singleton {
    Dmabuf,
    KeyboardExtension,
    AuraShell,
    Shm,
    TextInputManager,
    TextInputExtension,
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

    /// The binding that global `name` backs, if one does, with its
    /// singleton.
    pub(crate) fn bound_from(&self, name: u32) -> Option<(Singleton, Binding)> {
        self.0
            .iter()
            .find(|(_, binding)| binding.name == name)
            .map(|(&singleton, &binding)| (singleton, binding))
    }

    /// End the binding of `singleton`, if it has one.
    pub(crate) fn release(&mut self, singleton: Singleton) {
        self.0.remove(&singleton);
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
