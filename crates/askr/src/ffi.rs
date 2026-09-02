//! Helpers for the C boundary the PHP shim calls into.
//!
//! Every `pub extern "C"` entry point in this crate takes `(pointer, length)` pairs from
//! C and needs them as Rust slices. `std::slice::from_raw_parts` requires a non-null,
//! aligned pointer **even when the length is zero** — a null pointer there is undefined
//! behaviour, not an empty slice, which is one of the easier ways to get a
//! miscompilation rather than a crash.
//!
//! PHP can't currently produce one: the shim uses `Z_PARAM_STRING`, and a zend_string's
//! payload is never null (an empty string points at a valid NUL). But these functions are
//! `extern "C"` and reachable by anything that can link them, and the check costs a
//! branch that is never taken, so the boundary shouldn't rely on a caller's good manners.

/// Borrow `len` bytes from a C pointer, treating null as empty.
///
/// Empty is the right answer rather than a panic: an empty cache key misses, an empty
/// payload is refused by the callee, and unwinding across an FFI boundary would be worse
/// than either.
///
/// # Safety
/// When `ptr` is non-null it must be valid for reads of `len` bytes and must stay valid
/// (and unmutated) for the lifetime of the returned slice.
#[inline]
pub unsafe fn bytes<'a>(ptr: *const std::os::raw::c_char, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }
}

/// Run an FFI entry point's body, answering `fallback` if it panics.
///
/// A panic that unwinds out of an `extern "C"` function aborts the process: the worker
/// dies mid-request, the supervisor respawns it, and what the panic was about is
/// reported nowhere a person looks. None of these entry points has a reason to panic —
/// the point of the guard is that a bug in one costs a failed operation instead of a
/// killed worker, and says so in the log.
///
/// Shared-memory state stays consistent because the things that protect it are RAII and
/// run while unwinding: `Slot`'s guard releases the spinlock, and `Writing`'s guard
/// deliberately leaves its version counter *odd* when it is dropped during a panic, so a
/// half-written slot reads as busy rather than as stable.
///
/// Not for signal handlers: `catch_unwind` is not async-signal-safe, and the handlers in
/// `supervisor.rs` touch nothing but atomics anyway.
pub fn guard<T>(entry: &'static str, fallback: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            // The panic hook has already printed the message and location.
            tracing::error!(
                entry,
                "panic inside an FFI entry point — the operation failed and the worker \
                 survived. This is a bug; please report it."
            );
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    /// A panic unwinding out of an `extern "C"` function aborts the process. This is
    /// the whole reason `guard` exists, so it is worth a test that the panic really is
    /// contained and really is reported as a failed operation.
    #[test]
    fn a_panic_becomes_a_failed_operation() {
        // Quiet for the duration: the default hook would print a backtrace mid-test.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        assert_eq!(
            super::guard("test::int", 0, || 7),
            7,
            "no panic, no interference"
        );
        assert_eq!(
            super::guard("test::int", -1, || panic!("boom")),
            -1,
            "a panic must answer the fallback"
        );
        // The unit-returning shape the void trampolines use.
        super::guard("test::unit", (), || panic!("boom"));

        std::panic::set_hook(previous);
    }

    #[test]
    fn null_and_empty_are_empty_slices() {
        // The point of the exercise: no UB, no panic, a usable value.
        assert!(unsafe { super::bytes(std::ptr::null(), 0) }.is_empty());
        assert!(unsafe { super::bytes(std::ptr::null(), 16) }.is_empty());
        let s = b"hello";
        assert!(unsafe { super::bytes(s.as_ptr() as *const _, 0) }.is_empty());
    }

    #[test]
    fn a_real_pointer_is_borrowed_faithfully() {
        let s = b"askr";
        assert_eq!(unsafe { super::bytes(s.as_ptr() as *const _, 4) }, b"askr");
        assert_eq!(unsafe { super::bytes(s.as_ptr() as *const _, 2) }, b"as");
    }
}
