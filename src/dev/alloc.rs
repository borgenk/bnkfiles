//! A process-wide allocation counter for the performance gate. It wraps the system
//! allocator and bumps one relaxed atomic, so a scenario can report how many
//! allocations an operation cost, deterministically, beside its wall-clock time.
//! Allocation count is a clean signal that does not swing with inlining the way a
//! stopwatch does, so it is the metric the gate can trust across machines.
//!
//! It is installed as the global allocator only under the `perf-alloc` feature
//! (`make perf` enables it), because a global allocator taxes every allocation in
//! the whole process: the shipping viewer never pays for it. When the feature is
//! off the counter still exists but nothing writes it, so [`snapshot`] reads zero
//! and the gate prints the allocation column as unmeasured (that is what
//! [`ENABLED`] distinguishes).
//!
//! Only Rust-side allocations are counted. FreeType and HarfBuzz allocate on the C
//! side, invisibly to Rust's global allocator, so the glyph-heavy frame batch shows
//! fewer Rust allocations than its real cost by design.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether the counting allocator is actually installed (the `perf-alloc` feature
/// is on). The gate uses this to tell a measured zero from an unmeasured one.
#[cfg(feature = "perf-alloc")]
pub const ENABLED: bool = true;
#[cfg(not(feature = "perf-alloc"))]
pub const ENABLED: bool = false;

/// The counter is add-only within a scenario (reset zeroes it, alloc adds, dealloc
/// is not tracked), so it never underflows and is exactly reproducible for a given
/// code path and input, independent of the machine.
static ALLOCS: AtomicU64 = AtomicU64::new(0);

/// Zero the counter before measuring a scenario.
pub fn reset() {
    ALLOCS.store(0, Relaxed);
}

/// The number of allocation calls since the last [`reset`].
pub fn snapshot() -> u64 {
    ALLOCS.load(Relaxed)
}

/// The allocator: delegate every call to the system allocator, counting on the way
/// through. Installed as the global allocator only under `perf-alloc`.
#[cfg(feature = "perf-alloc")]
pub struct Counting;

#[cfg(feature = "perf-alloc")]
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = std::alloc::System.alloc(layout);
        if !ptr.is_null() {
            ALLOCS.fetch_add(1, Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout);
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = std::alloc::System.alloc_zeroed(layout);
        if !ptr.is_null() {
            ALLOCS.fetch_add(1, Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let new_ptr = std::alloc::System.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            // A realloc that grows is a fresh allocation's worth of work; count it
            // so growing a Vec in a loop shows the churn it really is.
            ALLOCS.fetch_add(1, Relaxed);
        }
        new_ptr
    }
}

#[cfg(feature = "perf-alloc")]
#[global_allocator]
static GLOBAL: Counting = Counting;
