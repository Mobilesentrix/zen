use std::ffi::CString;
use std::ptr::null_mut;

use libc::c_char;

use zen_engine::loader::LoaderError;
use zen_engine::EvaluationError;
use zen_expression::IsolateError;

use crate::error::{ZenError, ZenErrorDiscriminants};
use crate::ms_error::evaluation_error_value;

/// CResult can be seen as Either<Result, Error>. It cannot, and should not, be initialized
/// manually. Instead, use error or ok functions for initialisation.
#[repr(C)]
pub struct ZenResult<T> {
    result: *mut T,
    error: u8,
    details: *mut c_char,
}

impl<T> ZenResult<T> {
    pub(crate) fn error(error: ZenError) -> Self {
        let details = match error.details() {
            Some(data) => {
                let Ok(cstring_data) = CString::new(data) else {
                    return Self {
                        error: ZenErrorDiscriminants::StringNullError as u8,
                        result: null_mut(),
                        details: null_mut(),
                    };
                };

                crate::ms_alloc::malloc_cstring(cstring_data)
            }
            None => null_mut(),
        };

        return Self {
            result: null_mut(),
            error: ZenErrorDiscriminants::from(error) as u8,
            details,
        };
    }

    pub(crate) fn ok(result: *mut T) -> Self {
        return Self {
            result,
            error: ZenErrorDiscriminants::Zero as u8,
            details: null_mut(),
        };
    }
}

#[cfg(test)]
impl<T> ZenResult<T> {
    pub(crate) fn result_ptr(&self) -> *mut T {
        self.result
    }

    pub(crate) fn error_code(&self) -> u8 {
        self.error
    }

    pub(crate) fn details_ptr(&self) -> *const c_char {
        self.details
    }
}

impl<T> ZenResult<T> {
    /// An evaluation error as `details` JSON; `traced` adds the partial trace (see ms_error.rs).
    pub(crate) fn from_evaluation_error(error: &EvaluationError, traced: bool) -> Self {
        ZenResult::error(ZenError::EvaluationError(evaluation_error_value(
            error, traced,
        )))
    }
}

impl<T> From<&Box<EvaluationError>> for ZenResult<T> {
    fn from(evaluation_error: &Box<EvaluationError>) -> Self {
        ZenResult::from_evaluation_error(evaluation_error, false)
    }
}

impl<T> From<&IsolateError> for ZenResult<T> {
    fn from(isolate_error: &IsolateError) -> Self {
        let Ok(value) = serde_json::to_value(isolate_error) else {
            return ZenResult::error(ZenError::JsonSerializationFailed);
        };

        ZenResult::error(ZenError::IsolateError(value))
    }
}

impl<T> From<&LoaderError> for ZenResult<T> {
    fn from(loader_error: &LoaderError) -> Self {
        match loader_error {
            LoaderError::NotFound(key) => {
                ZenResult::error(ZenError::LoaderKeyNotFound { key: key.clone() })
            }
            LoaderError::Internal { source, key } => {
                ZenResult::error(ZenError::LoaderInternalError {
                    key: key.clone(),
                    message: source.to_string(),
                })
            }
        }
    }
}
