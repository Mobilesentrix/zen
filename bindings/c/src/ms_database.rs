//! MobileSentrix: engine constructors that install the `database` custom-node adapter
//! (`ms-zen-database`) in front of the caller's own custom-node handler.
//! Contract documented in MS-FFI.md at the repository root.

use crate::custom_node::{DynamicCustomNode, ZenCustomNodeResult};
use crate::engine::{ZenEngine, ZenEngineStruct};
use crate::error::ZenError;
use crate::guard::guarded;
use crate::helper::safe_str_from_ptr;
use crate::languages::native::NativeCustomNode;
use crate::loader::{DynamicDecisionLoader, ZenEngineLoaderConfig};
use crate::result::ZenResult;
use ms_zen_database::{DatabaseCustomNode, Databases, FallbackAdapter};
use std::ffi::c_char;
use std::sync::Arc;
use zen_engine::nodes::custom::NoopCustomNode;

/// Parses the connection-map JSON and creates the pools (once per engine).
pub(crate) fn database_node(
    database_config: *const c_char,
    fallback: FallbackAdapter,
) -> Result<DynamicCustomNode, ZenError> {
    let Some(json) = safe_str_from_ptr(database_config) else {
        return Err(ZenError::DatabaseConfigError {
            message: "database configuration JSON is required (null or not UTF-8)".to_string(),
        });
    };
    let databases = Databases::from_json(json).map_err(|e| ZenError::DatabaseConfigError {
        message: e.to_string(),
    })?;
    Ok(DynamicCustomNode::Database(
        DatabaseCustomNode::with_fallback(Arc::new(databases), fallback),
    ))
}

/// Creates an engine from a loader configuration with the database node installed.
/// `maybe_custom_node` handles every custom node whose kind is not "database" (may be null).
/// Caller frees the engine with zen_engine_free.
#[no_mangle]
pub extern "C" fn zen_engine_new_with_loader_config_and_database(
    config: ZenEngineLoaderConfig,
    maybe_custom_node: Option<extern "C" fn(request: *const c_char) -> ZenCustomNodeResult>,
    database_config: *const c_char,
) -> ZenResult<ZenEngineStruct> {
    guarded(|| {
        let loader = match config.to_dynamic_loader() {
            Ok(loader) => loader,
            Err(error) => return ZenResult::error(error),
        };
        let fallback: FallbackAdapter = match maybe_custom_node {
            Some(callback) => Box::new(NativeCustomNode::new(callback)),
            None => Box::new(NoopCustomNode),
        };
        let custom_node = match database_node(database_config, fallback) {
            Ok(node) => node,
            Err(error) => return ZenResult::error(error),
        };

        let engine = ZenEngine::new(DynamicDecisionLoader::Config(loader), custom_node);
        engine.compile();
        ZenResult::ok(Box::into_raw(Box::new(engine)) as *mut ZenEngineStruct)
    })
}

#[cfg(feature = "go")]
mod go {
    use super::*;
    use crate::languages::go::{map_handler, GoCustomNode, GoDecisionLoader};

    /// Go: like zen_engine_new_golang, plus the database node.
    /// `maybe_loader` / `maybe_custom_node` are cgo handles (null or 0 = none).
    /// Caller frees the engine with zen_engine_free.
    #[no_mangle]
    pub extern "C" fn zen_engine_new_golang_with_database(
        maybe_loader: Option<&usize>,
        maybe_custom_node: Option<&usize>,
        database_config: *const c_char,
    ) -> ZenResult<ZenEngineStruct> {
        guarded(|| {
            let loader = GoDecisionLoader::new(map_handler(maybe_loader.cloned()));
            let fallback = Box::new(GoCustomNode::new(map_handler(maybe_custom_node.cloned())));
            let custom_node = match database_node(database_config, fallback) {
                Ok(node) => node,
                Err(error) => return ZenResult::error(error),
            };

            let engine = ZenEngine::new(DynamicDecisionLoader::Go(loader), custom_node);
            ZenResult::ok(Box::into_raw(Box::new(engine)) as *mut ZenEngineStruct)
        })
    }

    /// Go: like zen_engine_new_golang_with_loader_config, plus the database node.
    /// Caller frees the engine with zen_engine_free.
    #[no_mangle]
    pub extern "C" fn zen_engine_new_golang_with_loader_config_and_database(
        config: ZenEngineLoaderConfig,
        maybe_custom_node: Option<&usize>,
        database_config: *const c_char,
    ) -> ZenResult<ZenEngineStruct> {
        guarded(|| {
            let loader = match config.to_dynamic_loader() {
                Ok(loader) => loader,
                Err(error) => return ZenResult::error(error),
            };
            let fallback = Box::new(GoCustomNode::new(map_handler(maybe_custom_node.cloned())));
            let custom_node = match database_node(database_config, fallback) {
                Ok(node) => node,
                Err(error) => return ZenResult::error(error),
            };

            let engine = ZenEngine::new(DynamicDecisionLoader::Config(loader), custom_node);
            engine.compile();
            ZenResult::ok(Box::into_raw(Box::new(engine)) as *mut ZenEngineStruct)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::zen_engine_free;
    use crate::loader::ZenLoaderConfigKind;
    use crate::mt::tokio_runtime;
    use std::ffi::CString;
    use std::ptr::null;

    fn static_config(content: &CString) -> ZenEngineLoaderConfig {
        ZenEngineLoaderConfig {
            kind: ZenLoaderConfigKind::Static,
            content: content.as_ptr(),
            bytes: null(),
            bytes_len: 0,
        }
    }

    #[test]
    fn invalid_database_config_returns_code_12() {
        let content = CString::new("{}").unwrap();
        for bad in [
            "not json",
            r#"{"connections":{"x":{"url":"oracle://h"}}}"#,
            r#"{"connections":{},"default":"missing"}"#,
            r#"{"connectionz":{}}"#,
        ] {
            let json = CString::new(bad).unwrap();
            let r = zen_engine_new_with_loader_config_and_database(
                static_config(&content),
                None,
                json.as_ptr(),
            );
            assert_eq!(r.error_code(), 12, "{bad}");
            assert!(r.result_ptr().is_null());
        }
        let r =
            zen_engine_new_with_loader_config_and_database(static_config(&content), None, null());
        assert_eq!(r.error_code(), 12);
    }

    #[test]
    fn engine_without_connections_still_evaluates_other_nodes() {
        let content = CString::new(format!(
            r#"{{"table.json": {}}}"#,
            include_str!("../../../test-data/table.json")
        ))
        .unwrap();
        let json = CString::new(r#"{"connections":{}}"#).unwrap();
        let r = zen_engine_new_with_loader_config_and_database(
            static_config(&content),
            None,
            json.as_ptr(),
        );
        assert_eq!(r.error_code(), 0);
        let engine = r.result_ptr();
        // SAFETY: `engine` was just returned by the constructor and is freed below.
        let zen_engine = unsafe { &*(engine as *const ZenEngine) };
        let input = zen_engine::Variable::from(serde_json::json!({"input": 12}));
        let v = tokio_runtime()
            .block_on(zen_engine.evaluate("table.json", input))
            .map(|r| serde_json::to_value(&r).unwrap())
            .unwrap();
        assert_eq!(v["result"]["output"], serde_json::json!(10));
        zen_engine_free(engine);
    }
}

#[cfg(test)]
mod error_details_tests {
    use crate::ms_error::evaluation_error_value;
    use ms_zen_database::ResultHandlingError;
    use serde_json::json;
    use zen_engine::EvaluationError;

    #[test]
    fn result_handling_node_errors_carry_structured_details() {
        let error = EvaluationError::NodeError {
            node_id: "db".into(),
            trace: None,
            source: Box::new(ResultHandlingError::several(
                "Duplicates found".to_string(),
                3,
                vec![json!({"id": 1}), json!({"id": 2})],
            )),
        };
        let value = evaluation_error_value(&error, false);
        assert_eq!(value["type"], json!("NodeError"));
        assert_eq!(value["nodeId"], json!("db"));
        assert_eq!(value["source"], json!("Duplicates found"));
        assert_eq!(
            value["details"],
            json!({"reason": "several", "rowsCount": 3, "rows": [{"id": 1}, {"id": 2}]})
        );

        // other node errors are serialised exactly as upstream (no details key)
        let plain = EvaluationError::NodeError {
            node_id: "x".into(),
            trace: None,
            source: "boom".into(),
        };
        let value = evaluation_error_value(&plain, false);
        assert_eq!(
            value,
            json!({"type": "NodeError", "source": "boom", "nodeId": "x"})
        );
    }
}
