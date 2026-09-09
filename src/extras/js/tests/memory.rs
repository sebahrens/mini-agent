use super::*;

#[test]
fn allocator_preserves_live_bytes_and_budget_on_failed_growth() {
    let failure = AllocationFailure(Rc::new(Cell::new(false)));
    let mut allocator = BoundedAllocator {
        limit: 64,
        used: 0,
        failure: failure.clone(),
    };
    // SAFETY: these pointers come from this allocator and are released exactly once.
    unsafe {
        let first = allocator.calloc(2, 12);
        assert!(!first.is_null());
        assert_eq!(allocator.used, 32);
        assert_eq!(BoundedAllocator::usable_size(first), 24);
        assert!(
            std::slice::from_raw_parts(first, 24)
                .iter()
                .all(|byte| *byte == 0)
        );
        first.write(91);
        let second = allocator.alloc(24);
        assert!(!second.is_null());
        assert_eq!(allocator.used, 64);
        assert!(allocator.realloc(first, 32).is_null());
        assert_eq!(allocator.used, 64);
        assert_eq!(first.read(), 91);
        assert!(failure.0.get());
        allocator.dealloc(second);
        let grown = allocator.realloc(first, 48);
        assert!(!grown.is_null());
        assert_eq!(grown.read(), 91);
        assert_eq!(allocator.used, 56);
        let shrunk = allocator.realloc(grown, 1);
        assert!(!shrunk.is_null());
        assert_eq!(shrunk.read(), 91);
        assert_eq!(allocator.used, 16);
        assert!(allocator.realloc(shrunk, 0).is_null());
        assert_eq!(allocator.used, 0);
        assert!(
            failure.0.get(),
            "freeing allocations must not erase OOM evidence"
        );
    }
}

#[test]
fn allocator_checks_overflow_alignment_and_zero_without_spurious_failure() {
    let failure = AllocationFailure(Rc::new(Cell::new(false)));
    let mut allocator = BoundedAllocator {
        limit: 32,
        used: 0,
        failure: failure.clone(),
    };
    assert!(allocator.calloc(0, usize::MAX).is_null());
    assert!(!failure.0.get());
    // SAFETY: null is explicitly supported; returned pointers are live until deallocated here.
    unsafe {
        allocator.dealloc(ptr::null_mut());
        assert_eq!(BoundedAllocator::usable_size(ptr::null_mut()), 0);
        let zero = allocator.alloc(0);
        assert!(!zero.is_null());
        allocator.dealloc(zero);
        let pointer = allocator.realloc(ptr::null_mut(), 17);
        assert!(!pointer.is_null());
        assert_eq!(pointer as usize % align_of::<Header>(), 0);
        assert_eq!(allocator.used, 32);
        assert!(allocator.realloc(pointer, usize::MAX).is_null());
        allocator.dealloc(pointer);
    }
    assert!(allocator.alloc(usize::MAX).is_null());
    assert!(allocator.calloc(usize::MAX, 2).is_null());
    assert_eq!(allocator.used, 0);
    assert!(failure.0.get());
}

#[test]
fn runtime_keeps_failure_across_unwinding_contexts_and_caught_exceptions() {
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    context.with(|ctx| {
        // No interrupt is installed here: prove the allocation record survives a caught native
        // exception even when evaluation itself succeeds and no large allocation remains live.
        assert!(
            ctx.eval::<bool, _>("try { new ArrayBuffer(128 * 1024 * 1024); } catch (_) {} true")
                .unwrap()
        );
        assert!(allocation_failed(&ctx));
        assert!(ensure_healthy(&ctx).is_err());
    });
    drop(context);
    assert!(runtime.allocation_failed());
    let sibling = Context::base(&runtime).unwrap();
    sibling.with(|ctx| assert!(allocation_failed(&ctx)));
    assert!(runtime.memory_usage().malloc_size < (MEMORY_LIMIT / 2) as i64);
    drop(sibling);
    drop(runtime);
    assert!(!Runtime::new().unwrap().allocation_failed());
}
