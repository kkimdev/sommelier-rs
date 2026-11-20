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

//! What the code on the current thread costs: heap allocations and their
//! bytes, and the system calls that copy pixels into, or map, a host copy.
//!
//! The test binary's global allocator counts every allocation on the thread
//! that makes it, and [`HostMapping`](crate::state::HostMapping) counts its
//! own system calls the same way. Tests run in parallel threads, so a test
//! reads only what its own thread did, by taking [`Counts::now`] before and
//! after the work it measures. A `#[tokio::test]` runs on its own thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ops::Sub;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
    static POOL_READS: Cell<u64> = const { Cell::new(0) };
    static HOST_MAPS: Cell<u64> = const { Cell::new(0) };
}

/// Add one to `counter`. A thread that is exiting may have lost its
/// counters; its allocations are not counted.
fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>) {
    add(counter, 1);
}

/// Add `n` to `counter`, as [`bump`] does one.
fn add(counter: &'static std::thread::LocalKey<Cell<u64>>, n: u64) {
    let _ = counter.try_with(|count| count.set(count.get() + n));
}

/// Count one allocation or reallocation of `size` bytes.
fn allocated(size: usize) {
    bump(&ALLOCATIONS);
    add(&ALLOCATED_BYTES, size as u64);
}

/// The system allocator, counting each allocation and reallocation on the
/// thread that makes it, and the bytes it asks for.
pub struct CountingAllocator;

// SAFETY: every method forwards to the system allocator unchanged. The
// counters are `const`-initialized thread locals without a destructor, so
// counting neither allocates nor reenters the allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        allocated(new_size);
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// One read system call (`pread` or `preadv`) that copies pool pixels into
/// a host copy.
pub fn count_pool_read() {
    bump(&POOL_READS);
}

/// One `mmap` of a host copy.
pub fn count_host_map() {
    bump(&HOST_MAPS);
}

/// Run `checks`, test-only work inside the code a test measures, such as
/// `Context::assert_no_orphans`, without counting what it does.
pub fn uncounted<R>(checks: impl FnOnce() -> R) -> R {
    let before = Counts::now();
    let result = checks();
    ALLOCATIONS.with(|count| count.set(before.allocations));
    ALLOCATED_BYTES.with(|count| count.set(before.allocated_bytes));
    POOL_READS.with(|count| count.set(before.pool_reads));
    HOST_MAPS.with(|count| count.set(before.host_maps));
    result
}

/// What the current thread has done so far, or between two readings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Heap allocations and reallocations.
    pub allocations: u64,
    /// The bytes they asked for: an allocation's size, and a
    /// reallocation's new size.
    pub allocated_bytes: u64,
    /// Read system calls into host copies.
    pub pool_reads: u64,
    /// Mappings of host copies.
    pub host_maps: u64,
}

impl Counts {
    /// What the current thread has done so far.
    pub fn now() -> Self {
        Self {
            allocations: ALLOCATIONS.with(Cell::get),
            allocated_bytes: ALLOCATED_BYTES.with(Cell::get),
            pool_reads: POOL_READS.with(Cell::get),
            host_maps: HOST_MAPS.with(Cell::get),
        }
    }

    /// What the current thread did while it ran `work`.
    pub fn of<R>(work: impl FnOnce() -> R) -> (Self, R) {
        let before = Self::now();
        let result = work();
        (Self::now() - before, result)
    }

    /// Each count divided by `n`, rounded up: the cost of one of `n`
    /// equal steps.
    pub fn per(self, n: u64) -> Self {
        Self {
            allocations: self.allocations.div_ceil(n),
            allocated_bytes: self.allocated_bytes.div_ceil(n),
            pool_reads: self.pool_reads.div_ceil(n),
            host_maps: self.host_maps.div_ceil(n),
        }
    }
}

impl Sub for Counts {
    type Output = Self;

    fn sub(self, earlier: Self) -> Self {
        Self {
            allocations: self.allocations - earlier.allocations,
            allocated_bytes: self.allocated_bytes - earlier.allocated_bytes,
            pool_reads: self.pool_reads - earlier.pool_reads,
            host_maps: self.host_maps - earlier.host_maps,
        }
    }
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.allocations += other.allocations;
        self.allocated_bytes += other.allocated_bytes;
        self.pool_reads += other.pool_reads;
        self.host_maps += other.host_maps;
    }
}

#[cfg(test)]
mod tests {
    use super::{uncounted, Counts};

    /// The counters see this thread's allocations, and only its own.
    // A test may wait for a thread; no client runs on the test's thread.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn counts_the_allocations_of_this_thread_only() {
        let (counts, boxed) = Counts::of(|| std::hint::black_box(Box::new(7u64)));
        assert_eq!((counts.allocations, counts.allocated_bytes), (1, 8));
        drop(boxed);
        // Spawning a thread allocates on this one, but far less than the
        // hundred vectors the thread allocates on its own.
        let (counts, ()) = Counts::of(|| {
            std::thread::spawn(|| {
                for _ in 0..100 {
                    drop(std::hint::black_box(vec![1u8; 64]));
                }
            })
            .join()
            .unwrap();
        });
        assert!(counts.allocations < 100, "{counts:?}");
        let (counts, ()) = Counts::of(|| uncounted(|| drop(std::hint::black_box(vec![1u8]))));
        assert_eq!(counts, Counts::default());
    }
}
