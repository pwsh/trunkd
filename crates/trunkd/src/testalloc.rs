// SPDX-License-Identifier: GPL-3.0-or-later
//! Test-only allocation counting.
//!
//! The unit-test binary runs on a global allocator that counts every
//! allocation and reallocation made **by the calling thread**, so a test can
//! pin how many heap operations a hot path makes without other tests running
//! in parallel disturbing the count. Only compiled into `cargo t`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    // `const` initialisation: no lazy init and no destructor, so reading it
    // from inside the allocator can never itself allocate.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn bump() {
    // `try_with`: the allocator runs during thread teardown too.
    let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to `System` unchanged; the counter is a
// thread-local `Cell` that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: forwarded with the caller's own layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f` and return its result with the number of allocations and
/// reallocations this thread made while it ran.
pub(crate) fn count<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = ALLOCS.with(Cell::get);
    let out = f();
    let after = ALLOCS.with(Cell::get);
    (out, after - before)
}
