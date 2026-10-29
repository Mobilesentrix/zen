//! MobileSentrix: compiled-decision cache control for engines created through the C API.
//! Contract documented in MS-FFI.md §1 ("Decision cache").

use crate::engine::{ZenEngine, ZenEngineStruct};
use crate::error::ZenErrorDiscriminants;
use crate::guard::guarded_code;
use crate::helper::safe_str_from_ptr;
use std::ffi::c_char;

/// Drops the cached decision for `key`; the next evaluation reloads it through the loader
/// (loader-config engines recompile their set). Returns 0, or 1 (InvalidArgument) when
/// `engine` or `key` is null or `key` is not UTF-8. `key` is only read during the call.
#[no_mangle]
pub extern "C" fn zen_engine_invalidate_decision(
    engine: *const ZenEngineStruct,
    key: *const c_char,
) -> u8 {
    guarded_code(|| {
        if engine.is_null() {
            return ZenErrorDiscriminants::InvalidArgument as u8;
        }
        let Some(key) = safe_str_from_ptr(key) else {
            return ZenErrorDiscriminants::InvalidArgument as u8;
        };
        // SAFETY: a non-null `engine` was returned by a zen_engine_new_* constructor and has not
        // been freed (caller contract, as for zen_engine_evaluate).
        let zen_engine = unsafe { &*(engine as *const ZenEngine) };
        zen_engine.invalidate_decision(key);
        ZenErrorDiscriminants::Zero as u8
    })
}

/// Drops every cached decision (rules refreshed, loader content replaced). Returns 0, or 1
/// (InvalidArgument) when `engine` is null.
#[no_mangle]
pub extern "C" fn zen_engine_clear_decisions(engine: *const ZenEngineStruct) -> u8 {
    guarded_code(|| {
        if engine.is_null() {
            return ZenErrorDiscriminants::InvalidArgument as u8;
        }
        // SAFETY: see zen_engine_invalidate_decision.
        let zen_engine = unsafe { &*(engine as *const ZenEngine) };
        zen_engine.clear_decisions();
        ZenErrorDiscriminants::Zero as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_node::ZenCustomNodeResult;
    use crate::engine::{
        zen_engine_evaluate, zen_engine_free, zen_engine_new_with_loader_config, ZenEngine,
        ZenEngineEvaluationOptions, ZenEngineStruct,
    };
    use crate::languages::native::zen_engine_new_native;
    use crate::loader::{ZenDecisionLoaderResult, ZenEngineLoaderConfig, ZenLoaderConfigKind};
    use serde_json::Value;
    use std::ffi::{c_char, CString};
    use std::ptr::null;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Number of times the native loader callback was invoked.
    static LOADS: AtomicUsize = AtomicUsize::new(0);
    /// The constant the served decision currently returns as `v`.
    static VALUE: Mutex<i64> = Mutex::new(1);

    fn graph(value: i64) -> String {
        format!(
            r#"{{"nodes":[
              {{"id":"in","name":"Request","type":"inputNode","position":{{"x":0,"y":0}}}},
              {{"id":"e","name":"e","type":"expressionNode","position":{{"x":0,"y":0}},
               "content":{{"expressions":[{{"id":"x","key":"v","value":"{value}"}}],"passThrough":false}}}},
              {{"id":"out","name":"Response","type":"outputNode","position":{{"x":0,"y":0}}}}],
             "edges":[{{"id":"e1","type":"edge","sourceId":"in","targetId":"e"}},
                      {{"id":"e2","type":"edge","sourceId":"e","targetId":"out"}}]}}"#
        )
    }

    extern "C" fn counting_loader(_key: *const c_char) -> ZenDecisionLoaderResult {
        LOADS.fetch_add(1, Ordering::SeqCst);
        ZenDecisionLoaderResult::from_content(&graph(*VALUE.lock().unwrap()))
    }

    extern "C" fn no_custom_nodes(_request: *const c_char) -> ZenCustomNodeResult {
        ZenCustomNodeResult::none()
    }

    fn evaluate_v(engine: *const ZenEngineStruct) -> Value {
        let key = CString::new("g.json").unwrap();
        let context = CString::new("{}").unwrap();
        let options = ZenEngineEvaluationOptions {
            trace: false,
            max_depth: 5,
        };
        let result = zen_engine_evaluate(engine, key.as_ptr(), context.as_ptr(), options);
        assert_eq!(result.error_code(), 0);
        // SAFETY: a zero error code means `result` is the C string the engine allocated for us.
        let response = unsafe { crate::ms_alloc::take_malloc_c_string(result.result_ptr()) };
        let value: Value = serde_json::from_slice(response.to_bytes()).unwrap();
        value["result"]["v"].clone()
    }

    #[test]
    fn callback_loader_decisions_are_cached_until_invalidated() {
        *VALUE.lock().unwrap() = 1;
        LOADS.store(0, Ordering::SeqCst);
        let engine = zen_engine_new_native(counting_loader, no_custom_nodes);

        assert_eq!(evaluate_v(engine), 1);
        assert_eq!(evaluate_v(engine), 1);
        assert_eq!(
            LOADS.load(Ordering::SeqCst),
            1,
            "second evaluation is a cache hit"
        );

        *VALUE.lock().unwrap() = 2;
        assert_eq!(evaluate_v(engine), 1, "cached until invalidated");
        let key = CString::new("g.json").unwrap();
        assert_eq!(zen_engine_invalidate_decision(engine, key.as_ptr()), 0);
        assert_eq!(evaluate_v(engine), 2, "no stale result after invalidation");
        assert_eq!(LOADS.load(Ordering::SeqCst), 2);

        *VALUE.lock().unwrap() = 3;
        assert_eq!(zen_engine_clear_decisions(engine), 0);
        assert_eq!(evaluate_v(engine), 3);
        assert_eq!(LOADS.load(Ordering::SeqCst), 3);

        // argument validation: code 1 (InvalidArgument), never a crash
        assert_eq!(zen_engine_invalidate_decision(null(), key.as_ptr()), 1);
        assert_eq!(zen_engine_invalidate_decision(engine, null()), 1);
        assert_eq!(zen_engine_clear_decisions(null()), 1);
        let bad_utf8 = CString::new([0xffu8, 0xfe]).unwrap();
        assert_eq!(zen_engine_invalidate_decision(engine, bad_utf8.as_ptr()), 1);

        zen_engine_free(engine);
    }

    #[test]
    fn loader_config_engines_build_the_compiled_set() {
        // "broken.json" fails graph validation (edge to a node that does not exist), so a built
        // compiled set reports exactly one failure (a wrapper hiding `keys` would make `compile()` see none).
        let content = CString::new(format!(
            r#"{{"g.json": {}, "broken.json": {{"nodes": [], "edges": [{{"id": "e", "type": "edge", "sourceId": "x", "targetId": "y"}}]}}}}"#,
            graph(7)
        ))
        .unwrap();
        let config = ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Static,
            content: content.as_ptr(),
            bytes: null(),
            bytes_len: 0,
        };
        let result = zen_engine_new_with_loader_config(config, None);
        assert_eq!(result.error_code(), 0);
        let engine = result.result_ptr();
        // SAFETY: `engine` was just returned by the constructor and is freed below.
        let zen_engine = unsafe { &*(engine as *const ZenEngine) };
        let failures = zen_engine.compile_failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].key.as_ref(), "broken.json");

        assert_eq!(evaluate_v(engine), 7);
        // invalidation on a loader-config engine recompiles the set; still consistent
        let key = CString::new("g.json").unwrap();
        assert_eq!(zen_engine_invalidate_decision(engine, key.as_ptr()), 0);
        assert_eq!(zen_engine_clear_decisions(engine), 0);
        assert_eq!(zen_engine.compile_failures().len(), 1);
        assert_eq!(evaluate_v(engine), 7);

        zen_engine_free(engine);
    }
}
