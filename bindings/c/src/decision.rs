use std::ffi::{c_char, c_void, CStr};
use std::marker::{PhantomData, PhantomPinned};
use std::ops::{Deref, DerefMut};
use zen_engine::Decision;

use crate::engine::ZenEngineEvaluationOptions;
use crate::error::ZenError;
use crate::guard::{guarded, guarded_unit};
use crate::ms_context::{parse_context, OutOfRange};
use crate::mt::tokio_runtime;
use crate::result::ZenResult;
use ms_zen_database::with_evaluation_scope;

#[repr(C)]
pub(crate) struct ZenDecision {
    _data: Decision,
    _marker: PhantomData<(*mut c_void, PhantomPinned)>,
}

impl Deref for ZenDecision {
    type Target = Decision;

    fn deref(&self) -> &Self::Target {
        &self._data
    }
}

impl DerefMut for ZenDecision {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self._data
    }
}

impl From<Decision> for ZenDecision {
    fn from(value: Decision) -> Self {
        Self {
            _data: value,
            _marker: PhantomData,
        }
    }
}

#[repr(C)]
pub(crate) struct ZenDecisionStruct {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Frees ZenDecision
#[no_mangle]
pub extern "C" fn zen_decision_free(decision: *mut ZenDecisionStruct) {
    guarded_unit(|| {
        let _ = unsafe { Box::from_raw(decision as *mut ZenDecision) };
    })
}

/// Evaluates ZenDecision
/// Caller is responsible for freeing context and ZenResult.
#[no_mangle]
pub extern "C" fn zen_decision_evaluate(
    decision: *const ZenDecisionStruct,
    context_ptr: *const c_char,
    options: ZenEngineEvaluationOptions,
) -> ZenResult<c_char> {
    guarded(|| {
        if decision.is_null() || context_ptr.is_null() {
            return ZenResult::error(ZenError::InvalidArgument);
        }

        let cstr_context = unsafe { CStr::from_ptr(context_ptr) };
        let Ok(str_context) = cstr_context.to_str() else {
            return ZenResult::error(ZenError::InvalidArgument);
        };

        // One pass into a Variable (same tree as Value + Variable::from).
        let Ok(context) = parse_context(str_context.as_bytes(), OutOfRange::Null) else {
            return ZenResult::error(ZenError::JsonDeserializationFailed);
        };

        let zen_decision = unsafe { &*(decision as *mut ZenDecision) };
        let traced = options.trace;
        let maybe_result = tokio_runtime().block_on(with_evaluation_scope(
            zen_decision.evaluate_with_opts(context, options.into()),
        ));
        let result = match maybe_result {
            Ok(r) => r,
            Err(e) => return ZenResult::from_evaluation_error(&e, traced),
        };

        // Written straight into malloc memory (ms_alloc::malloc_json).
        match crate::ms_alloc::malloc_json(&result) {
            Ok(p) => ZenResult::ok(p),
            Err(_) => ZenResult::error(ZenError::JsonSerializationFailed),
        }
    })
}
