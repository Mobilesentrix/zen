//! Panic guard for every `extern "C"` entry point.
//!
//! A panic that reaches the C boundary aborts the whole process, so every exported function
//! runs its body under `catch_unwind`; a panic becomes error code 13 (`Panic`) with the
//! panic message in `details`, a null pointer for the raw-pointer constructors, or a no-op
//! for the `*_free` functions. The engine stays usable afterwards: its shared state is
//! behind `Arc`/`RwLock` and poisoned locks are tolerated by the code that uses them.

use crate::error::{ZenError, ZenErrorDiscriminants};
use crate::result::ZenResult;
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::null_mut;

pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic with a non-string payload".to_string()
    }
}

/// Runs `body`; a panic becomes `ZenError::Panic` (code 13).
pub(crate) fn guarded<T>(body: impl FnOnce() -> ZenResult<T>) -> ZenResult<T> {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|payload| {
        ZenResult::error(ZenError::Panic {
            message: panic_message(payload),
        })
    })
}

/// Runs `body`; a panic becomes a null pointer (constructors without a `ZenResult`).
pub(crate) fn guarded_ptr<T>(body: impl FnOnce() -> *mut T) -> *mut T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(null_mut())
}

/// Runs `body`; a panic becomes error code 13 (functions returning a bare code).
pub(crate) fn guarded_code(body: impl FnOnce() -> u8) -> u8 {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(ZenErrorDiscriminants::Panic as u8)
}

/// Runs `body`; a panic is swallowed (`*_free` functions).
pub(crate) fn guarded_unit(body: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(body));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_char, CStr};

    fn details(result: &ZenResult<c_char>) -> String {
        // SAFETY: `details` is either null or a C string this crate allocated.
        unsafe { CStr::from_ptr(result.details_ptr()) }
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn panic_becomes_error_code_13_with_the_message() {
        let result: ZenResult<c_char> = guarded(|| panic!("boom at {}", 42));
        assert_eq!(result.error_code(), 13);
        assert!(result.result_ptr().is_null());
        assert_eq!(details(&result), r#"{"message":"boom at 42"}"#);

        let result: ZenResult<c_char> = guarded(|| panic!("static"));
        assert_eq!(details(&result), r#"{"message":"static"}"#);
    }

    #[test]
    fn panic_becomes_null_pointer_or_code_and_is_swallowed_for_free() {
        let ptr: *mut u8 = guarded_ptr(|| panic!("ctor"));
        assert!(ptr.is_null());
        assert_eq!(guarded_code(|| panic!("code")), 13);
        guarded_unit(|| panic!("free"));
        // and the happy paths pass values through unchanged
        assert_eq!(guarded_code(|| 0), 0);
        let value = Box::into_raw(Box::new(7u8));
        let same: *mut u8 = guarded_ptr(|| value);
        assert_eq!(same, value);
        // SAFETY: `value` was just leaked by Box::into_raw above.
        drop(unsafe { Box::from_raw(value) });
    }
}
