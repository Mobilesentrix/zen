use crate::custom_node::{
    NoopCustomNodeCallback, ZenCustomNodeCallback, ZenCustomNodeCallbackWrapper,
};
use crate::database::database_adapter;
use crate::decision::ZenDecision;
use crate::error::ZenError;
use crate::loader::{
    ContentReader, NoopDecisionLoader, ZenDecisionLoaderCallbackWrapper, ZenLoader,
};
use crate::types::{JsonBuffer, ZenBatchRequest, ZenBatchResult, ZenEngineResponse};
use ms_zen_database::{FallbackAdapter, InstanceRouting, with_evaluation_scope};
use serde_json::Value;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::task;
use zen_engine::loader::DynamicLoader;
use zen_engine::nodes::custom::DynamicCustomNode;
use zen_engine::{DecisionEngine, EvaluationOptions};

#[derive(uniffi::Object)]
pub(crate) struct ZenEngine {
    engine: Arc<DecisionEngine>,
    /// How `create_decision` reads content (the loader reads stored decisions the same way).
    reader: ContentReader,
}

#[derive(uniffi::Record)]
pub struct ZenEvaluateOptions {
    pub max_depth: Option<u8>,
    pub trace: Option<bool>,
}

impl Default for ZenEvaluateOptions {
    fn default() -> Self {
        Self {
            max_depth: Some(5),
            trace: Some(false),
        }
    }
}

impl From<ZenEvaluateOptions> for EvaluationOptions {
    fn from(value: ZenEvaluateOptions) -> Self {
        Self {
            max_depth: value.max_depth.unwrap_or(5),
            trace: value.trace.unwrap_or(false),
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl ZenEngine {
    #[uniffi::constructor(default(loader = None, custom_node = None))]
    pub fn new(
        loader: Option<ZenLoader>,
        custom_node: Option<Box<dyn ZenCustomNodeCallback>>,
    ) -> Result<Self, ZenError> {
        let custom_node: DynamicCustomNode = Arc::new(callback_adapter(custom_node));
        Self::build(loader, custom_node, ContentReader::Plain)
    }

    /// An engine that runs rules saved by the Rules Engine editor, Lookup (database) steps
    /// included, exactly as the Rules Engine runs them: every decision the loader returns, and
    /// the content given to `create_decision`, has its Lookup steps (`databaseNode`) converted
    /// into the engine's database node.
    ///
    /// `database_config` is the connection map JSON (`{"connections": {"<instance>": {"url":
    /// "mysql://…", "aliases": [...]}}, "default": "<instance>"}`); with none, a Lookup step
    /// fails with "database node not available". `route_by_instance` true runs each step on the
    /// connection (or alias) named by its editor instance; false runs every step on the default
    /// connection. `custom_node` handles every other custom node.
    #[uniffi::constructor(default(
        loader = None,
        custom_node = None,
        database_config = None,
        route_by_instance = false
    ))]
    pub fn with_database(
        loader: Option<ZenLoader>,
        custom_node: Option<Box<dyn ZenCustomNodeCallback>>,
        database_config: Option<String>,
        route_by_instance: bool,
    ) -> Result<Self, ZenError> {
        let routing = if route_by_instance {
            InstanceRouting::ByInstance
        } else {
            InstanceRouting::DefaultConnection
        };
        let fallback: FallbackAdapter = Box::new(callback_adapter(custom_node));
        let custom_node = database_adapter(database_config.as_deref(), fallback)?;
        Self::build(loader, custom_node, ContentReader::Editor(routing))
    }

    pub async fn evaluate(
        &self,
        key: String,
        context: JsonBuffer,
        options: Option<ZenEvaluateOptions>,
    ) -> Result<ZenEngineResponse, ZenError> {
        let options = options.unwrap_or_default();
        let context: Value = context.try_into()?;

        let engine = self.engine.clone();

        // Use spawn_blocking to run the non-Send code synchronously
        let response = task::spawn_blocking(move || {
            // The blocking code that uses non-Send types
            Handle::current().block_on(async move {
                let context = zen_engine::Variable::try_from_value(context)
                    .map_err(|e| ZenError::ValidationError(e.to_string()))?;
                with_evaluation_scope(engine.evaluate_with_opts(key, context, options.into()))
                    .await
                    .map(|response| ZenEngineResponse::try_from(response))
                    .map_err(|err| {
                        ZenError::EvaluationError(
                            serde_json::to_string(&err.as_ref())
                                .unwrap_or_else(|_| err.to_string()),
                        )
                    })
            })
        })
        .await
        .map_err(|e| ZenError::EvaluationError(format!("Task failed: {:?}", e)))???;

        Ok(response)
    }

    pub async fn evaluate_batch(
        &self,
        requests: Vec<ZenBatchRequest>,
        options: Option<ZenEvaluateOptions>,
    ) -> Vec<ZenBatchResult> {
        let options: EvaluationOptions = options.unwrap_or_default().into();

        let handles: Vec<_> = requests
            .into_iter()
            .map(|request| {
                let engine = self.engine.clone();
                task::spawn_blocking(move || {
                    Handle::current().block_on(async move {
                        let context: Value = request.context.try_into()?;
                        let context = zen_engine::Variable::try_from_value(context)
                            .map_err(|e| ZenError::ValidationError(e.to_string()))?;
                        let response = with_evaluation_scope(engine.evaluate_with_opts(
                            request.key,
                            context,
                            options,
                        ))
                        .await
                        .map_err(|err| {
                            ZenError::EvaluationError(
                                serde_json::to_string(&err.as_ref())
                                    .unwrap_or_else(|_| err.to_string()),
                            )
                        })?;

                        ZenEngineResponse::try_from(response)
                    })
                })
            })
            .collect();

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let result = match handle.await {
                Ok(Ok(data)) => ZenBatchResult {
                    success: true,
                    data: Some(data),
                    error: None,
                },
                Ok(Err(error)) => ZenBatchResult {
                    success: false,
                    data: None,
                    error: Some(error.details()),
                },
                Err(_) => ZenBatchResult {
                    success: false,
                    data: None,
                    error: Some("evaluation worker panicked".to_string()),
                },
            };
            results.push(result);
        }

        results
    }

    pub fn create_decision(&self, content: JsonBuffer) -> Result<ZenDecision, ZenError> {
        let content = match self.reader {
            ContentReader::Plain => serde_json::from_slice(&content.0)
                .map_err(|_| ZenError::JsonDeserializationFailed)?,
            ContentReader::Editor(_) => self
                .reader
                .read(&content.0)
                .map_err(|e| ZenError::ValidationError(e.to_string()))?,
        };
        let decision = self
            .engine
            .create_decision(Arc::new(content))
            .map_err(|e| ZenError::ValidationError(e.to_string()))?;

        Ok(ZenDecision::from(decision))
    }

    pub async fn get_decision(&self, key: String) -> Result<ZenDecision, ZenError> {
        let engine = self.engine.clone();

        // Use spawn_blocking to run the non-Send code synchronously
        let decision =
            task::spawn_blocking(move || {
                // The blocking code that uses non-Send types
                Handle::current().block_on(async move {
                    let outer = engine.get_decision(&key).await.map_err(|e| {
                        ZenError::LoaderInternalError {
                            key: key.clone(),
                            details: e.to_string(),
                        }
                    })?;
                    outer
                        .map_err(|e| ZenError::ValidationError(e.to_string()))
                        .map(ZenDecision::from)
                })
            })
            .await
            .map_err(|e| ZenError::EvaluationError(format!("Task failed: {:?}", e)))??;

        Ok(decision)
    }
}

impl ZenEngine {
    fn build(
        loader: Option<ZenLoader>,
        custom_node: DynamicCustomNode,
        reader: ContentReader,
    ) -> Result<Self, ZenError> {
        let loader: DynamicLoader = match loader {
            Some(loader) => loader.into_dynamic_loader_with(reader)?,
            None => Arc::new(ZenDecisionLoaderCallbackWrapper::new(
                Arc::new(NoopDecisionLoader),
                reader,
            )),
        };

        let engine = DecisionEngine::new(loader, custom_node);
        engine.compile();

        Ok(Self {
            engine: Arc::new(engine),
            reader,
        })
    }
}

/// The foreign custom-node handler (or the no-op one) as an engine adapter.
fn callback_adapter(
    custom_node: Option<Box<dyn ZenCustomNodeCallback>>,
) -> ZenCustomNodeCallbackWrapper {
    ZenCustomNodeCallbackWrapper(custom_node.unwrap_or_else(|| Box::new(NoopCustomNodeCallback)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::ZenLoader;
    use std::collections::HashMap;

    async fn assert_table_output(engine: &ZenEngine) {
        let response = engine
            .evaluate(
                "table.json".to_string(),
                JsonBuffer(br#"{"input":12}"#.to_vec()),
                None,
            )
            .await
            .unwrap();

        let result: Value = response.result.try_into().unwrap();
        assert_eq!(result["output"], serde_json::json!(10));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_static_loader_config() {
        let content = HashMap::from([(
            "table.json".to_string(),
            JsonBuffer(include_bytes!("../../../test-data/table.json").to_vec()),
        )]);

        let engine = ZenEngine::new(Some(ZenLoader::Static { content }), None).unwrap();
        assert_table_output(&engine).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_filesystem_loader_config() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-data").to_string();
        let engine = ZenEngine::new(Some(ZenLoader::Filesystem { path }), None).unwrap();
        assert_table_output(&engine).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_callback_loader() {
        struct FsCallback;

        #[async_trait::async_trait]
        impl crate::loader::ZenDecisionLoaderCallback for FsCallback {
            async fn load(&self, key: String) -> Result<Option<JsonBuffer>, ZenError> {
                let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-data");
                Ok(std::fs::read(format!("{path}/{key}")).ok().map(JsonBuffer))
            }
        }

        let engine = ZenEngine::new(
            Some(ZenLoader::Callback {
                callback: Arc::new(FsCallback),
            }),
            None,
        )
        .unwrap();
        assert_table_output(&engine).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evaluate_batch_mixed_results() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-data").to_string();
        let engine = ZenEngine::new(Some(ZenLoader::Filesystem { path }), None).unwrap();

        let requests = vec![
            ZenBatchRequest {
                key: "table.json".to_string(),
                context: JsonBuffer(br#"{"input":12}"#.to_vec()),
            },
            ZenBatchRequest {
                key: "missing.json".to_string(),
                context: JsonBuffer(b"{}".to_vec()),
            },
            ZenBatchRequest {
                key: "table.json".to_string(),
                context: JsonBuffer(br#"{"input":5}"#.to_vec()),
            },
        ];

        let results = engine.evaluate_batch(requests, None).await;
        assert_eq!(results.len(), 3);

        assert!(results[0].success);
        let first: Value =
            serde_json::from_slice(&results[0].data.as_ref().unwrap().result.0).unwrap();
        assert_eq!(first["output"], serde_json::json!(10));

        assert!(!results[1].success);
        assert!(results[1].error.is_some());

        assert!(results[2].success);
        let third: Value =
            serde_json::from_slice(&results[2].data.as_ref().unwrap().result.0).unwrap();
        assert_eq!(third["output"], serde_json::json!(0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_static_loader_config_missing_key() {
        let engine = ZenEngine::new(
            Some(ZenLoader::Static {
                content: HashMap::new(),
            }),
            None,
        )
        .unwrap();

        let result = engine
            .evaluate("missing.json".to_string(), JsonBuffer(b"{}".to_vec()), None)
            .await;
        assert!(result.is_err());
    }
}
