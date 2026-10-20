//! Test-only counting allocator. Counts allocations made by the *current
//! thread* between `measure` entry and exit, so concurrently running tests do
//! not disturb each other.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

fn record(size: usize) {
    // `try_with` so allocation during thread teardown never panics.
    let _ = ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = COUNT.try_with(|count| count.set(count.get() + 1));
            let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
        }
    });
}

// SAFETY: every method delegates to `System` with the caller's layout; the
// bookkeeping only touches const-initialised thread-local `Cell`s.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `work` and returns its result with `(allocation count, bytes
/// requested)` made on this thread.
pub(crate) fn measure<T>(work: impl FnOnce() -> T) -> (T, usize, usize) {
    COUNT.with(|count| count.set(0));
    BYTES.with(|bytes| bytes.set(0));
    ENABLED.with(|enabled| enabled.set(true));
    let result = work();
    ENABLED.with(|enabled| enabled.set(false));
    (result, COUNT.with(Cell::get), BYTES.with(Cell::get))
}
