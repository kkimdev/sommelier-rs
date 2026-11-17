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

use std::ops::Range;

/// libwayland's object-map limit (`WL_MAP_MAX_OBJECTS` in
/// `wayland-private.h`). `wl_map_insert_at` refuses an ID whose index
/// within its side's map exceeds it (`i > WL_MAP_MAX_OBJECTS`), so the
/// highest usable index is this value itself. A client ID is its own index;
/// a server ID's index is its offset from `0xff000000`.
pub(super) const WL_MAP_MAX_OBJECTS: u32 = 0x00f0_0000;

/// Recycles the Wayland object IDs that one side of a connection allocates.
///
/// Like libwayland's object map, a released ID is reused before a fresh one
/// (last released, first reused), and fresh IDs are handed out in order. The
/// IDs in use therefore stay as dense as the live objects: libwayland
/// refuses a new ID beyond its map's end and caps the map size, so a
/// counter that only grows ends the session after enough object churn.
///
/// The owner decides when an ID is released; this type only guarantees that
/// an ID is not handed out again until then.
#[derive(Debug)]
pub(super) struct IdAllocator {
    range: Range<u32>,
    next: u32,
    free: Vec<u32>,
}

impl IdAllocator {
    pub(super) fn new(range: Range<u32>) -> Self {
        Self {
            next: range.start,
            range,
            free: Vec::new(),
        }
    }

    /// Whether `id` lies in the range this allocator hands out.
    pub(super) fn contains(&self, id: u32) -> bool {
        self.range.contains(&id)
    }

    /// Take the next candidate ID: the last released one, else the next
    /// fresh one. `None` once the range is exhausted.
    pub(super) fn next_candidate(&mut self) -> Option<u32> {
        self.free.pop().or_else(|| {
            let id = self.next;
            (id < self.range.end).then(|| {
                self.next += 1;
                id
            })
        })
    }

    /// Return `id`, which is no longer in use, for reuse.
    ///
    /// An ID this allocator never handed out is ignored: whoever chose it
    /// owns its reuse, and reusing it here could exceed the dense range.
    pub(super) fn release(&mut self, id: u32) {
        if self.contains(id) && id < self.next {
            debug_assert!(!self.free.contains(&id), "ID {id} released twice");
            self.free.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IdAllocator;

    #[test]
    fn released_ids_are_reused_before_fresh_ones() {
        let mut ids = IdAllocator::new(2..10);
        assert_eq!(ids.next_candidate(), Some(2));
        assert_eq!(ids.next_candidate(), Some(3));
        assert_eq!(ids.next_candidate(), Some(4));
        ids.release(3);
        ids.release(2);
        assert_eq!(ids.next_candidate(), Some(2));
        assert_eq!(ids.next_candidate(), Some(3));
        assert_eq!(ids.next_candidate(), Some(5));
    }

    #[test]
    fn ids_never_handed_out_are_not_released() {
        let mut ids = IdAllocator::new(2..10);
        assert_eq!(ids.next_candidate(), Some(2));
        ids.release(1);
        ids.release(3);
        ids.release(10);
        assert_eq!(ids.next_candidate(), Some(3));
        assert_eq!(ids.next_candidate(), Some(4));
    }

    #[test]
    fn exhausted_range_yields_no_candidate() {
        let mut ids = IdAllocator::new(2..4);
        assert_eq!(ids.next_candidate(), Some(2));
        assert_eq!(ids.next_candidate(), Some(3));
        assert_eq!(ids.next_candidate(), None);
        ids.release(2);
        assert_eq!(ids.next_candidate(), Some(2));
        assert_eq!(ids.next_candidate(), None);
    }
}
