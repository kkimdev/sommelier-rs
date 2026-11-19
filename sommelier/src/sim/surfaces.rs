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

//! Which field a surface is. A field's window can close and open again on a
//! new surface (F2), so a field has a surface per generation, on the guest
//! and on the host. The world, the display, the specification and the
//! shadow share one map ([`Surfaces`]) and look every surface up in it, so
//! that a surface of an earlier generation still names its field.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

/// The surfaces of each field, oldest first; cloning shares the map.
#[derive(Clone, Debug, Default)]
pub(super) struct Surfaces(Rc<RefCell<Map>>);

/// The map: each field's surfaces, and the surfaces the guest has
/// destroyed: on the guest, whose IDs it never reuses
/// (`Session::new_id`), and on the host, until the proxy reuses the ID.
#[derive(Debug, Default)]
struct Map {
    fields: Vec<FieldSurfaces>,
    guests_destroyed: BTreeSet<u32>,
    hosts_destroyed: BTreeSet<u32>,
}

/// One field's surfaces, a generation each.
#[derive(Clone, Debug, Default)]
struct FieldSurfaces {
    guest: Vec<u32>,
    host: Vec<u32>,
}

impl Surfaces {
    /// Fields on guest surfaces `guest` and host surfaces `host`, a
    /// generation each; `host` may be shorter until the host has them.
    pub fn new(guest: &[u32], host: &[u32]) -> Self {
        let fields = guest
            .iter()
            .enumerate()
            .map(|(i, &guest)| FieldSurfaces {
                guest: vec![guest],
                host: host.get(i).copied().into_iter().collect(),
            })
            .collect();
        Self(Rc::new(RefCell::new(Map {
            fields,
            ..Map::default()
        })))
    }

    /// A new field, on guest surface `guest`.
    pub fn add_field(&self, guest: u32) {
        let field = FieldSurfaces {
            guest: vec![guest],
            host: Vec::new(),
        };
        self.0.borrow_mut().fields.push(field);
    }

    /// Field `field`'s guest surface now.
    pub fn guest(&self, field: usize) -> u32 {
        *self.0.borrow().fields[field]
            .guest
            .last()
            .expect("a guest surface")
    }

    /// Field `field`'s host surface now: none until the host has one, nor
    /// from the destroy of its last one until the proxy creates the next.
    pub fn host(&self, field: usize) -> Option<u32> {
        let map = self.0.borrow();
        let host = map.fields[field].host.last().copied();
        host.filter(|host| !map.hosts_destroyed.contains(host))
    }

    /// The field guest surface `surface` is or was, if any.
    pub fn field_of_guest(&self, surface: u32) -> Option<usize> {
        let map = self.0.borrow();
        map.fields
            .iter()
            .position(|field| field.guest.contains(&surface))
    }

    /// The field host surface `surface` is or was, if any.
    pub fn field_of_host(&self, surface: u32) -> Option<usize> {
        let map = self.0.borrow();
        map.fields
            .iter()
            .position(|field| field.host.contains(&surface))
    }

    /// Field `field` has guest surface `surface` from now on.
    pub fn add_guest(&self, field: usize, surface: u32) {
        self.0.borrow_mut().fields[field].guest.push(surface);
    }

    /// Field `field` has host surface `surface` from now on; with no
    /// field, the first that has none yet, as the proxy creates them in the
    /// guest's order. The proxy reuses the host ID of a destroyed object,
    /// so an earlier generation that had `surface` no longer names it.
    pub fn add_host(&self, field: Option<usize>, surface: u32) {
        let mut map = self.0.borrow_mut();
        map.hosts_destroyed.remove(&surface);
        for earlier in map.fields.iter_mut() {
            earlier.host.retain(|&host| host != surface);
        }
        let field = field.or_else(|| map.fields.iter().position(|field| field.host.is_empty()));
        map.fields[field.expect("a field for the host surface")]
            .host
            .push(surface);
    }

    /// The proxy got the guest's destroy of guest surface `surface`, a
    /// field's: its host surface is gone too, until the proxy reuses the
    /// ID. That is the field's last host surface, since the proxy creates
    /// the next only at the guest's next request, which follows the
    /// destroy.
    ///
    /// # Panics
    ///
    /// If `surface` is no field's.
    pub fn destroy(&self, surface: u32) {
        let field = self.field_of_guest(surface).expect("a field's surface");
        let mut map = self.0.borrow_mut();
        map.guests_destroyed.insert(surface);
        if let Some(&host) = map.fields[field].host.last() {
            map.hosts_destroyed.insert(host);
        }
    }

    /// Whether guest surface `surface` is one the guest destroyed.
    pub fn guest_destroyed(&self, surface: u32) -> bool {
        self.0.borrow().guests_destroyed.contains(&surface)
    }

    /// Whether host surface `surface` is one the guest destroyed.
    pub fn host_destroyed(&self, surface: u32) -> bool {
        self.0.borrow().hosts_destroyed.contains(&surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_keeps_the_surfaces_of_its_generations() {
        let surfaces = Surfaces::new(&[30, 31], &[]);
        let shared = surfaces.clone();
        surfaces.add_host(None, 20);
        surfaces.add_host(None, 21);
        assert_eq!((shared.host(0), shared.host(1)), (Some(20), Some(21)));
        shared.add_guest(1, 32);
        surfaces.add_host(Some(1), 22);
        assert_eq!((surfaces.guest(1), surfaces.host(1)), (32, Some(22)));
        assert_eq!(surfaces.field_of_guest(31), Some(1), "an earlier one");
        assert_eq!(surfaces.field_of_host(22), Some(1));
        assert_eq!(surfaces.field_of_host(99), None);
        // The destroy of a field's surface takes its host surface too, and
        // the field has none until the next.
        surfaces.destroy(32);
        assert!(surfaces.guest_destroyed(32) && !surfaces.guest_destroyed(31));
        assert!(surfaces.host_destroyed(22));
        assert_eq!(surfaces.host(1), None);
        surfaces.add_guest(1, 33);
        surfaces.add_host(Some(1), 23);
        assert_eq!(surfaces.host(1), Some(23));
        // A host ID the proxy reuses names its new field only, and is no
        // longer destroyed.
        surfaces.add_host(Some(0), 22);
        assert!(!surfaces.host_destroyed(22));
        assert_eq!(surfaces.field_of_host(22), Some(0));
        assert_eq!(surfaces.host(0), Some(22));
    }
}
