use serde_json::Value;
use std::ffi::{c_char, CStr};
use std::marker::{PhantomData, PhantomPinned};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use zen_engine::loader::{CachedLoader, DynamicLoader};
use zen_engine::model::DecisionContent;
use zen_engine::{Decision, DecisionEngine, EvaluationOptions};

use crate::custom_node::DynamicCustomNode;
use crate::custom_node::ZenCustomNodeResult;
use crate::decision::{ZenDecision, ZenDecisionStruct};
use crate::error::ZenError;
use crate::guard::{guarded, guarded_ptr, guarded_unit};
use crate::helper::safe_str_from_ptr;
use crate::languages::native::NativeCustomNode;
use crate::loader::{DynamicDecisionLoader, ZenEngineLoaderConfig};
use crate::ms_context::{parse_context, OutOfRange};
use crate::ms_created::CreatedDecisionCache;
use crate::ms_error::evaluation_error_value;
use crate::mt::{tokio_runtime, worker_pool};
use crate::result::ZenResult;
use ms_zen_database::with_evaluation_scope;
use serde_json::json;

pub(crate) struct ZenEngine {
    engine: DecisionEngine,
    /// Present for callback loaders (Go / native): caches the decisions they return, since
    /// the engine's compiled set only covers loaders that can enumerate their keys.
    cache: Option<Arc<CachedLoader>>,
    /// Prepared (compiled) graphs of decisions created from JSON, keyed by their text.
    created: CreatedDecisionCache,
}

impl Deref for ZenEngine {
    type Target = DecisionEngine;

    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

impl DerefMut for ZenEngine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.engine
    }
}

impl Default for ZenEngine {
    fn default() -> Self {
        Self::new(
            DynamicDecisionLoader::default(),
            DynamicCustomNode::default(),
        )
    }
}

impl ZenEngine {
    pub fn new(loader: DynamicDecisionLoader, custom_node: DynamicCustomNode) -> Self {
        crate::ms_alloc::start_purger();
        let (loader, cache): (DynamicLoader, Option<Arc<CachedLoader>>) = if loader.is_callback() {
            let cached = Arc::new(CachedLoader::from(Arc::new(loader) as DynamicLoader));
            (cached.clone(), Some(cached))
        } else {
            (Arc::new(loader), None)
        };
        Self {
            engine: DecisionEngine::new(loader, Arc::new(custom_node)),
            cache,
            created: CreatedDecisionCache::new(),
        }
    }

    /// Creates a decision from JSON text, prepared (compiled, schemas resolved) like a loaded
    /// one; the prepared graph is reused for later creates of the same text.
    pub fn create_prepared_decision(&self, source: &str) -> Result<Decision, ZenError> {
        let parse = |text: &str| {
            let content: DecisionContent =
                serde_json::from_str(text).map_err(|_| ZenError::JsonDeserializationFailed)?;
            match content {
                DecisionContent::Graph(graph) => Ok(graph),
                // valid JSON, but a policy document is not a decision graph (code 1, as upstream)
                DecisionContent::Policy(_) => Err(ZenError::InvalidArgument),
            }
        };
        let prepared = tokio_runtime().block_on(self.created.prepared(
            &self.engine.loader(),
            source,
            parse,
        ))?;
        self.engine
            .create_decision(Arc::new(DecisionContent::Graph(prepared)))
            .map_err(|_| ZenError::InvalidArgument)
    }

    /// Forgets the cached decision for `key` (callback loaders) or rebuilds the compiled set
    /// from the loader (loader-config engines).
    pub fn invalidate_decision(&self, key: &str) {
        match &self.cache {
            Some(cache) => {
                cache.invalidate(key);
            }
            None => {
                self.engine.compile();
            }
        }
    }

    /// Forgets every cached decision, including the prepared graphs of created decisions.
    pub fn clear_decisions(&self) {
        self.created.clear();
        match &self.cache {
            Some(cache) => cache.clear(),
            None => {
                self.engine.compile();
            }
        }
    }
}

#[repr(C)]
pub(crate) struct ZenEngineStruct {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

#[repr(C)]
pub struct ZenEngineEvaluationOptions {
    pub(crate) trace: bool,
    pub(crate) max_depth: u8,
}

impl Into<EvaluationOptions> for ZenEngineEvaluationOptions {
    fn into(self) -> EvaluationOptions {
        EvaluationOptions {
            trace: self.trace,
            max_depth: self.max_depth,
        }
    }
}

/// Create a new ZenEngine instance, caller is responsible for freeing the returned reference
/// by calling zen_engine_free.
#[no_mangle]
pub extern "C" fn zen_engine_new() -> *mut ZenEngineStruct {
    guarded_ptr(|| Box::into_raw(Box::new(ZenEngine::default())) as *mut ZenEngineStruct)
}

/// Creates a new ZenEngine instance from a loader configuration, caller is responsible for
/// freeing the returned reference by calling zen_engine_free.
#[no_mangle]
pub extern "C" fn zen_engine_new_with_loader_config(
    config: ZenEngineLoaderConfig,
    maybe_custom_node: Option<extern "C" fn(request: *const c_char) -> ZenCustomNodeResult>,
) -> ZenResult<ZenEngineStruct> {
    guarded(|| {
        let loader = match config.to_dynamic_loader() {
            Ok(loader) => loader,
            Err(error) => return ZenResult::error(error),
        };

        let custom_node = match maybe_custom_node {
            Some(callback) => DynamicCustomNode::Native(NativeCustomNode::new(callback)),
            None => DynamicCustomNode::default(),
        };

        let engine = ZenEngine::new(DynamicDecisionLoader::Config(loader), custom_node);
        engine.compile();

        ZenResult::ok(Box::into_raw(Box::new(engine)) as *mut ZenEngineStruct)
    })
}

/// Frees the ZenEngine instance reference from the memory
#[no_mangle]
pub extern "C" fn zen_engine_free(engine: *mut ZenEngineStruct) {
    guarded_unit(|| {
        if !engine.is_null() {
            let _ = unsafe { Box::from_raw(engine as *mut ZenEngine) };
        }
    })
}

/// Creates a Decision using a reference of DecisionEngine and content (JSON)
/// Caller is responsible for freeing content and ZenResult.
#[no_mangle]
pub extern "C" fn zen_engine_create_decision(
    engine: *const ZenEngineStruct,
    content: *const c_char,
) -> ZenResult<ZenDecisionStruct> {
    guarded(|| {
        if engine.is_null() || content.is_null() {
            return ZenResult::error(ZenError::InvalidArgument);
        }

        let Some(source) = safe_str_from_ptr(content) else {
            return ZenResult::error(ZenError::JsonDeserializationFailed);
        };

        let zen_engine = unsafe { &*(engine as *mut ZenEngine) };
        let decision = match zen_engine.create_prepared_decision(source) {
            Ok(d) => d,
            Err(e) => return ZenResult::error(e),
        };

        let zen_decision = ZenDecision::from(decision);
        ZenResult::ok(Box::into_raw(Box::new(zen_decision)) as *mut ZenDecisionStruct)
    })
}

/// Evaluates rules engine using a DecisionEngine reference via loader
/// Caller is responsible for freeing: key, context and ZenResult.
#[no_mangle]
pub extern "C" fn zen_engine_evaluate(
    engine: *const ZenEngineStruct,
    key: *const c_char,
    context: *const c_char,
    options: ZenEngineEvaluationOptions,
) -> ZenResult<c_char> {
    guarded(|| {
        if engine.is_null() || key.is_null() || context.is_null() {
            return ZenResult::error(ZenError::InvalidArgument);
        }

        let Some(str_key) = safe_str_from_ptr(key) else {
            return ZenResult::error(ZenError::InvalidArgument);
        };

        let cstr_context = unsafe { CStr::from_ptr(context) };
        // One pass into a Variable (same tree as Value + try_from_value).
        let Ok(var_context) = parse_context(cstr_context.to_bytes(), OutOfRange::Error) else {
            return ZenResult::error(ZenError::JsonDeserializationFailed);
        };

        let zen_engine = unsafe { &*(engine as *mut ZenEngine) };

        let traced = options.trace;
        let maybe_result = tokio_runtime().block_on(with_evaluation_scope(
            zen_engine.evaluate_with_opts(str_key, var_context, options.into()),
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

#[repr(C)]
pub struct ZenEngineEvaluateBatchRequest {
    key: *const c_char,
    context: *const c_char,
}

enum BatchTask {
    Failed(serde_json::Value),
    Pending(tokio::task::JoinHandle<Result<serde_json::Value, serde_json::Value>>),
}

/// Evaluates a batch of requests in parallel using a DecisionEngine reference via loader.
/// Returns a JSON array of { success, data?, error? } envelopes in request order.
/// Caller is responsible for freeing: requests and ZenResult.
#[no_mangle]
pub extern "C" fn zen_engine_evaluate_batch(
    engine: *const ZenEngineStruct,
    requests: *const ZenEngineEvaluateBatchRequest,
    requests_len: usize,
    options: ZenEngineEvaluationOptions,
) -> ZenResult<c_char> {
    guarded(|| {
        if engine.is_null() || (requests.is_null() && requests_len > 0) {
            return ZenResult::error(ZenError::InvalidArgument);
        }

        let request_slice = match requests_len {
            0 => &[],
            _ => unsafe { std::slice::from_raw_parts(requests, requests_len) },
        };

        let mut parsed: Vec<(String, Result<Value, String>)> = Vec::with_capacity(requests_len);
        for request in request_slice {
            let Some(key) = safe_str_from_ptr(request.key) else {
                return ZenResult::error(ZenError::InvalidArgument);
            };

            if request.context.is_null() {
                return ZenResult::error(ZenError::InvalidArgument);
            }

            let cstr_context = unsafe { CStr::from_ptr(request.context) };
            let context =
                serde_json::from_slice::<Value>(cstr_context.to_bytes()).map_err(|e| e.to_string());
            parsed.push((key.to_string(), context));
        }

        let zen_engine = unsafe { &*(engine as *const ZenEngine) };
        let decision_engine: DecisionEngine = DecisionEngine::clone(zen_engine);
        let eval_options: EvaluationOptions = options.into();

        let pool = worker_pool();
        let tasks: Vec<BatchTask> = parsed
            .into_iter()
            .map(|(key, context)| match context {
                Err(message) => BatchTask::Failed(json!(format!("invalid context: {message}"))),
                Ok(value) => {
                    let engine = decision_engine.clone();
                    BatchTask::Pending(pool.spawn_pinned(move || async move {
                        let value = zen_engine::Variable::try_from_value(value)
                            .map_err(|e| json!(format!("invalid context: {e}")))?;
                        with_evaluation_scope(engine.evaluate_with_opts(key, value, eval_options))
                            .await
                            .map(|response| serde_json::to_value(&response).unwrap_or(Value::Null))
                            .map_err(|e| evaluation_error_value(&e, eval_options.trace))
                    }))
                }
            })
            .collect();

        let results = tokio_runtime().block_on(async move {
            let mut items = Vec::with_capacity(tasks.len());
            for task in tasks {
                let item = match task {
                    BatchTask::Failed(error) => json!({ "success": false, "error": error }),
                    BatchTask::Pending(handle) => match handle.await {
                        Ok(Ok(data)) => json!({ "success": true, "data": data }),
                        Ok(Err(error)) => json!({ "success": false, "error": error }),
                        Err(_) => {
                            json!({ "success": false, "error": "evaluation worker panicked" })
                        }
                    },
                };
                items.push(item);
            }
            Value::Array(items)
        });

        match crate::ms_alloc::malloc_json(&results) {
            Ok(p) => ZenResult::ok(p),
            Err(_) => ZenResult::error(ZenError::JsonSerializationFailed),
        }
    })
}

/// Loads a Decision through DecisionEngine
/// Caller is responsible for freeing: key and ZenResult.
#[no_mangle]
pub extern "C" fn zen_engine_get_decision(
    engine: *const ZenEngineStruct,
    key: *const c_char,
) -> ZenResult<ZenDecisionStruct> {
    guarded(|| {
        if engine.is_null() || key.is_null() {
            return ZenResult::error(ZenError::InvalidArgument);
        }

        let cstr_key = unsafe { CStr::from_ptr(key) };
        let Ok(str_key) = cstr_key.to_str() else {
            return ZenResult::error(ZenError::InvalidArgument);
        };

        let zen_engine = unsafe { &*(engine as *mut ZenEngine) };
        let decision = match tokio_runtime().block_on(zen_engine.get_decision(str_key)) {
            Ok(Ok(d)) => d,
            Ok(Err(_)) => return ZenResult::error(ZenError::InvalidArgument),
            Err(e) => return ZenResult::from(&e),
        };

        let zen_decision = ZenDecision::from(decision);
        ZenResult::ok(Box::into_raw(Box::new(zen_decision)) as *mut ZenDecisionStruct)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ZenErrorDiscriminants;
    use crate::loader::ZenLoaderConfigKind;
    use std::ffi::CString;
    use std::ptr::null;

    fn evaluate_table(engine: *mut ZenEngineStruct) -> Value {
        let key = CString::new("table.json").unwrap();
        let context = CString::new(r#"{"input":12}"#).unwrap();
        let result = zen_engine_evaluate(
            engine,
            key.as_ptr(),
            context.as_ptr(),
            ZenEngineEvaluationOptions {
                trace: false,
                max_depth: 5,
            },
        );

        assert_eq!(result.error_code(), 0);
        let response = unsafe { crate::ms_alloc::take_malloc_c_string(result.result_ptr()) };
        serde_json::from_slice(response.to_bytes()).unwrap()
    }

    #[test]
    fn engine_from_static_loader_config() {
        let content = CString::new(format!(
            r#"{{"table.json": {}}}"#,
            include_str!("../../../test-data/table.json")
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
        let response = evaluate_table(engine);
        assert_eq!(response["result"]["output"], serde_json::json!(10));

        zen_engine_free(engine);
    }

    #[test]
    fn engine_from_fs_loader_config() {
        let path = CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-data")).unwrap();

        let config = ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Filesystem,
            content: path.as_ptr(),
            bytes: null(),
            bytes_len: 0,
        };

        let result = zen_engine_new_with_loader_config(config, None);
        assert_eq!(result.error_code(), 0);

        let engine = result.result_ptr();
        let response = evaluate_table(engine);
        assert_eq!(response["result"]["output"], serde_json::json!(10));

        zen_engine_free(engine);
    }

    #[test]
    fn engine_evaluate_batch_mixed_results() {
        let path = CString::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-data")).unwrap();
        let config = ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Filesystem,
            content: path.as_ptr(),
            bytes: null(),
            bytes_len: 0,
        };
        let engine = zen_engine_new_with_loader_config(config, None).result_ptr();

        let keys = [
            CString::new("table.json").unwrap(),
            CString::new("missing.json").unwrap(),
            CString::new("table.json").unwrap(),
        ];
        let contexts = [
            CString::new(r#"{"input":12}"#).unwrap(),
            CString::new(r#"{}"#).unwrap(),
            CString::new(r#"{"input":5}"#).unwrap(),
        ];
        let requests: Vec<ZenEngineEvaluateBatchRequest> = keys
            .iter()
            .zip(contexts.iter())
            .map(|(key, context)| ZenEngineEvaluateBatchRequest {
                key: key.as_ptr(),
                context: context.as_ptr(),
            })
            .collect();

        let result = zen_engine_evaluate_batch(
            engine,
            requests.as_ptr(),
            requests.len(),
            ZenEngineEvaluationOptions {
                trace: false,
                max_depth: 5,
            },
        );

        assert_eq!(result.error_code(), 0);
        let response = unsafe { crate::ms_alloc::take_malloc_c_string(result.result_ptr()) };
        let items: Value = serde_json::from_slice(response.to_bytes()).unwrap();

        assert_eq!(items[0]["success"], serde_json::json!(true));
        assert_eq!(items[0]["data"]["result"]["output"], serde_json::json!(10));
        assert_eq!(items[1]["success"], serde_json::json!(false));
        assert_eq!(items[2]["success"], serde_json::json!(true));
        assert_eq!(items[2]["data"]["result"]["output"], serde_json::json!(0));

        zen_engine_free(engine);
    }

    #[test]
    fn created_decision_matches_loader_path_and_reuses_compiled_graph() {
        let table = include_str!("../../../test-data/table.json");
        let content = CString::new(format!(r#"{{"table.json": {table}}}"#)).unwrap();
        let config = ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Static,
            content: content.as_ptr(),
            bytes: null(),
            bytes_len: 0,
        };
        let engine = zen_engine_new_with_loader_config(config, None).result_ptr();
        let expected = evaluate_table(engine)["result"].clone();

        let source = CString::new(table).unwrap();
        let context = CString::new(r#"{"input":12}"#).unwrap();
        let options = || ZenEngineEvaluationOptions {
            trace: false,
            max_depth: 5,
        };
        let evaluate_created = |decision: *mut ZenDecisionStruct| -> Value {
            let result =
                crate::decision::zen_decision_evaluate(decision, context.as_ptr(), options());
            assert_eq!(result.error_code(), 0);
            let response = unsafe { crate::ms_alloc::take_malloc_c_string(result.result_ptr()) };
            let value: Value = serde_json::from_slice(response.to_bytes()).unwrap();
            value["result"].clone()
        };

        let first = zen_engine_create_decision(engine, source.as_ptr());
        assert_eq!(first.error_code(), 0);
        assert_eq!(evaluate_created(first.result_ptr()), expected);
        let second = zen_engine_create_decision(engine, source.as_ptr());
        assert_eq!(second.error_code(), 0);
        assert_eq!(evaluate_created(second.result_ptr()), expected);
        crate::decision::zen_decision_free(first.result_ptr());
        crate::decision::zen_decision_free(second.result_ptr());

        // SAFETY: `engine` came from the constructor above and is freed below.
        let zen_engine = unsafe { &*(engine as *const ZenEngine) };
        assert_eq!(
            zen_engine.created.hits(),
            1,
            "second create reused the graph"
        );

        assert_eq!(crate::ms_cache::zen_engine_clear_decisions(engine), 0);
        let third = zen_engine_create_decision(engine, source.as_ptr());
        assert_eq!(third.error_code(), 0);
        assert_eq!(zen_engine.created.hits(), 1, "cleared: compiled again");
        assert_eq!(evaluate_created(third.result_ptr()), expected);
        crate::decision::zen_decision_free(third.result_ptr());

        let bad = CString::new("{not json").unwrap();
        assert_eq!(
            zen_engine_create_decision(engine, bad.as_ptr()).error_code(),
            ZenErrorDiscriminants::JsonDeserializationFailed as u8
        );
        let policy = CString::new(r#"{"blocks": []}"#).unwrap();
        assert_eq!(
            zen_engine_create_decision(engine, policy.as_ptr()).error_code(),
            ZenErrorDiscriminants::InvalidArgument as u8,
            "policy document is not a graph"
        );
        assert_eq!(
            zen_engine_create_decision(null(), source.as_ptr()).error_code(),
            ZenErrorDiscriminants::InvalidArgument as u8
        );

        zen_engine_free(engine);
    }

    /// A failed traced evaluation must carry the trace collected up to the failing
    /// node (completed steps + the failing node's input), not `"trace": null`.
    #[test]
    fn traced_evaluation_error_carries_the_partial_trace() {
        // in -> ok (k = 1) -> fail (missing.price - cost, nulls in arithmetic) -> out
        let content = CString::new(
            r#"{"fail.json": {"nodes": [
              {"id":"in","name":"Request","type":"inputNode","position":{"x":0,"y":0}},
              {"id":"ok","name":"ok","type":"expressionNode","position":{"x":0,"y":0},
               "content":{"expressions":[{"id":"a","key":"k","value":"1"}],"passThrough":true}},
              {"id":"fail","name":"fail","type":"expressionNode","position":{"x":0,"y":0},
               "content":{"expressions":[{"id":"b","key":"margin","value":"missing.price - cost"}],"passThrough":false}},
              {"id":"out","name":"Response","type":"outputNode","position":{"x":0,"y":0}}],
             "edges":[{"id":"e1","type":"edge","sourceId":"in","targetId":"ok"},
                      {"id":"e2","type":"edge","sourceId":"ok","targetId":"fail"},
                      {"id":"e3","type":"edge","sourceId":"fail","targetId":"out"}]}}"#,
        )
        .unwrap();
        let config = ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Static,
            content: content.as_ptr(),
            bytes: null(),
            bytes_len: 0,
        };
        let engine = zen_engine_new_with_loader_config(config, None).result_ptr();
        let key = CString::new("fail.json").unwrap();
        let context = CString::new(r#"{"sku":"107000001022"}"#).unwrap();
        let evaluate = |trace: bool| -> Value {
            let result = zen_engine_evaluate(
                engine,
                key.as_ptr(),
                context.as_ptr(),
                ZenEngineEvaluationOptions {
                    trace,
                    max_depth: 5,
                },
            );
            assert_eq!(
                result.error_code(),
                ZenErrorDiscriminants::EvaluationError as u8
            );
            // SAFETY: a non-zero code means `details` is the C string the engine allocated.
            let details = unsafe {
                crate::ms_alloc::take_malloc_c_string(result.details_ptr() as *mut c_char)
            };
            serde_json::from_slice(details.to_bytes()).unwrap()
        };

        let traced = evaluate(true);
        assert_eq!(traced["type"], json!("NodeError"));
        assert_eq!(traced["nodeId"], json!("fail"));
        let steps = traced["trace"]
            .as_object()
            .unwrap_or_else(|| panic!("trace missing: {traced}"));
        assert!(
            steps.contains_key("in") && steps.contains_key("ok"),
            "{traced}"
        );
        assert_eq!(steps["ok"]["output"]["k"], json!(1), "completed step kept");
        assert_eq!(
            steps["fail"]["input"]["sku"],
            json!("107000001022"),
            "failing node's input kept"
        );
        assert_eq!(steps["fail"]["input"]["k"], json!(1));
        assert!(!steps.contains_key("out"), "nothing after the failure");
        // the failing step says what failed and why; completed steps have no `error`
        assert_eq!(
            steps["fail"]["error"],
            json!({
                "kind": "missingValue",
                "message": "VM error: Opcode Subtract: Unsupported type",
                "part": "expression",
                "expression": "missing.price - cost",
                "key": "margin",
                "missing": ["missing.price", "cost"]
            }),
            "{traced}"
        );
        assert!(steps["ok"].get("error").is_none(), "{traced}");
        assert_eq!(steps["ok"]["traceData"], json!({"k": {"result": 1}}));

        let untraced = evaluate(false);
        assert!(
            untraced.get("trace").is_none() || untraced["trace"].is_null(),
            "{untraced}"
        );

        zen_engine_free(engine);
    }

    #[test]
    fn engine_from_invalid_zip_loader_config() {
        let bytes = [0u8; 4];
        let config = ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Zip,
            content: null(),
            bytes: bytes.as_ptr(),
            bytes_len: bytes.len(),
        };

        let result = zen_engine_new_with_loader_config(config, None);
        assert_eq!(
            result.error_code(),
            ZenErrorDiscriminants::LoaderConfigError as u8
        );
        assert!(result.result_ptr().is_null());
    }
}
