//! Worker-local QuickJS allocation accounting and sticky allocation-failure evidence.

#![allow(unsafe_code)]

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use std::cell::Cell;
use std::ops::Deref;
use std::ptr;
use std::rc::Rc;

use rquickjs::{Context, Ctx, Error, JsLifetime, allocator::Allocator};

use super::types::MEMORY_LIMIT;

#[derive(Clone)]
struct AllocationFailure(Rc<Cell<bool>>);

// SAFETY: the flag contains only Rust-owned data, with no JavaScript lifetime or handles.
unsafe impl<'js> JsLifetime<'js> for AllocationFailure {
    type Changed<'to> = Self;
}

pub(crate) fn allocation_failed(ctx: &Ctx<'_>) -> bool {
    ctx.userdata::<AllocationFailure>()
        .is_some_and(|failure| failure.0.get())
}

pub(crate) fn ensure_healthy(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    if allocation_failed(ctx) {
        Err(Error::Allocation)
    } else {
        Ok(())
    }
}

/// Owns the allocator from runtime construction, including allocations made by new contexts.
/// Do not set QuickJS's separate native memory limit: it can reject allocations before calling
/// this allocator, hiding the failure. The allocator itself enforces the complete heap budget.
pub(crate) struct Runtime {
    inner: rquickjs::Runtime,
    failure: AllocationFailure,
}

impl Runtime {
    pub(crate) fn new() -> rquickjs::Result<Self> {
        let failure = AllocationFailure(Rc::new(Cell::new(false)));
        let inner = rquickjs::Runtime::new_with_alloc(BoundedAllocator {
            limit: MEMORY_LIMIT,
            used: 0,
            failure: failure.clone(),
        })?;
        // Userdata belongs to the runtime and is shared by every later private/model context.
        Context::base(&inner)?.with(|ctx| {
            ctx.store_userdata(failure.clone())
                .map_err(|_| Error::Unknown)
        })?;
        Ok(Self { inner, failure })
    }

    pub(crate) fn allocation_failed(&self) -> bool {
        self.failure.0.get()
    }

    pub(crate) fn set_interrupt_handler(
        &self,
        handler: Option<rquickjs::runtime::InterruptHandler>,
    ) {
        let failure = self.failure.clone();
        let mut handler = handler;
        self.inner.set_interrupt_handler(Some(Box::new(move || {
            failure.0.get() || handler.as_mut().is_some_and(|handler| handler())
        })));
    }
}

impl Deref for Runtime {
    type Target = rquickjs::Runtime;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// QuickJS requires u64 alignment. Include the header and alignment padding in the budget.
#[repr(C, align(8))]
struct Header {
    size: usize,
}

struct BoundedAllocator {
    limit: usize,
    used: usize,
    failure: AllocationFailure,
}

impl BoundedAllocator {
    fn reject(&self) -> *mut u8 {
        self.failure.0.set(true);
        ptr::null_mut()
    }

    fn layout(&self, size: usize, old_size: usize) -> Option<Layout> {
        let size = size.checked_add(size_of::<Header>())?;
        let layout = Layout::from_size_align(size, align_of::<Header>())
            .ok()?
            .pad_to_align();
        (self
            .used
            .checked_sub(old_size)?
            .checked_add(layout.size())?
            <= self.limit)
            .then_some(layout)
    }

    fn allocate(&mut self, size: usize, zeroed: bool) -> *mut u8 {
        let Some(layout) = self.layout(size, 0) else {
            return self.reject();
        };
        // SAFETY: layout is nonzero, checked for overflow, and suitably aligned for Header/u64.
        let base = unsafe {
            if zeroed {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };
        if base.is_null() {
            return self.reject();
        }
        self.used += layout.size();
        // SAFETY: the allocation contains the aligned header followed by the requested payload.
        unsafe {
            base.cast::<Header>().write(Header {
                size: layout.size(),
            });
            base.add(size_of::<Header>())
        }
    }
}

// SAFETY: all successful allocations carry their full layout size in an aligned private header.
// Reallocation preserves the old allocation/accounting on failure; deallocation uses that exact
// layout. Null/zero inputs and all size arithmetic are handled before accessing memory.
unsafe impl Allocator for BoundedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        self.allocate(size, false)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        if count == 0 || size == 0 {
            return ptr::null_mut();
        }
        let Some(size) = count.checked_mul(size) else {
            return self.reject();
        };
        self.allocate(size, true)
    }

    unsafe fn dealloc(&mut self, pointer: *mut u8) {
        if pointer.is_null() {
            return;
        }
        unsafe {
            let base = pointer.sub(size_of::<Header>());
            let size = (*base.cast::<Header>()).size;
            self.used -= size;
            dealloc(
                base,
                Layout::from_size_align_unchecked(size, align_of::<Header>()),
            );
        }
    }

    unsafe fn realloc(&mut self, pointer: *mut u8, new_size: usize) -> *mut u8 {
        if pointer.is_null() {
            return self.alloc(new_size);
        }
        if new_size == 0 {
            unsafe {
                self.dealloc(pointer);
            }
            return ptr::null_mut();
        }
        unsafe {
            let base = pointer.sub(size_of::<Header>());
            let old_size = (*base.cast::<Header>()).size;
            let Some(layout) = self.layout(new_size, old_size) else {
                return self.reject();
            };
            let old_layout = Layout::from_size_align_unchecked(old_size, align_of::<Header>());
            let new_base = realloc(base, old_layout, layout.size());
            if new_base.is_null() {
                return self.reject();
            }
            self.used = self.used - old_size + layout.size();
            new_base.cast::<Header>().write(Header {
                size: layout.size(),
            });
            new_base.add(size_of::<Header>())
        }
    }

    unsafe fn usable_size(pointer: *mut u8) -> usize {
        if pointer.is_null() {
            return 0;
        }
        unsafe { (*pointer.sub(size_of::<Header>()).cast::<Header>()).size - size_of::<Header>() }
    }
}

#[cfg(test)]
#[path = "tests/memory.rs"]
mod tests;
