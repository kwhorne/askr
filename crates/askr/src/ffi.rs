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
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }
}

#[cfg(test)]
mod tests {
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
