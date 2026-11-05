//! `CustomNodeAdapter` for custom nodes of kind `"database"`.
//!
//! JDM node:
//! ```json
//! { "type": "customNode", "id": "db", "name": "rows",
//!   "content": { "kind": "database", "config": {
//!       "instance": "<connection name>",          // optional when a default exists
//!       "query": "SELECT ... WHERE sku IN ({{ skus }})",
//!       "outputPath": "rows", "passThrough": true,  // defaults: "dbResult", true
//!       "inputField": "order", "executionMode": "single" } } }
//! ```
//! `useCustomQuery` + `customQuery` (the editor's raw shape) are accepted when `query` is absent.
//! Any other kind is delegated to the fallback adapter (e.g. the Go custom-node callback).

use crate::bind::NodeLabel;
use crate::connection::{Connection, PoolKind};
use crate::databases::Databases;
use crate::query_step::{unshare, QueryStep};
use crate::result_handling::{shape_result, ResultHandling};
use crate::template::{Dialect, QueryTemplate};
use hashlink::LruCache;
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest, NoopCustomNode};
use zen_engine::nodes::failure::{FailedNode, FailureKind, FailurePart, NodeFailure};
use zen_engine::nodes::{NodeError, NodeResponse, NodeResult};
use zen_expression::{Isolate, Variable};

pub const DATABASE_NODE_KIND: &str = "database";

/// Parsed templates kept per dialect (LRU).
const TEMPLATE_CACHE_CAPACITY: usize = 1024;

const DEFAULT_OUTPUT_PATH: &str = "dbResult";

pub type FallbackAdapter = Box<dyn CustomNodeAdapter + Send + Sync>;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionMode {
    #[default]
    Single,
    Loop,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeConfig {
    #[serde(default)]
    result_handling: Option<ResultHandling>,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    use_custom_query: bool,
    #[serde(default)]
    custom_query: Option<String>,
    #[serde(default)]
    output_path: Option<String>,
    #[serde(default)]
    pass_through: Option<bool>,
    #[serde(default)]
    input_field: Option<String>,
    #[serde(default)]
    execution_mode: ExecutionMode,
}

impl NodeConfig {
    fn sql(&self) -> Option<&str> {
        let query = self.query.as_deref().filter(|q| !q.trim().is_empty());
        query.or_else(|| {
            self.use_custom_query
                .then_some(self.custom_query.as_deref())
                .flatten()
                .filter(|q| !q.trim().is_empty())
        })
    }

    /// An absent or empty `outputPath` means [`DEFAULT_OUTPUT_PATH`].
    fn output_path(&self) -> &str {
        match self.output_path.as_deref() {
            None | Some("") => DEFAULT_OUTPUT_PATH,
            Some(path) => path,
        }
    }

    fn input_field(&self) -> Option<&str> {
        self.input_field
            .as_deref()
            .filter(|field| !field.is_empty())
    }
}

type TemplateCache = Mutex<LruCache<String, Arc<QueryTemplate>>>;

/// The database node adapter. One per engine; holds the pools and the parsed-query cache.
pub struct DatabaseCustomNode {
    databases: Arc<Databases>,
    fallback: FallbackAdapter,
    /// Indexed by [`cache_slot`]: one bounded cache per dialect, looked up by `&str`.
    templates: [TemplateCache; 2],
}

fn cache_slot(dialect: Dialect) -> usize {
    match dialect {
        Dialect::MySql => 0,
        Dialect::Postgres => 1,
    }
}

impl fmt::Debug for DatabaseCustomNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseCustomNode")
            .field("databases", &self.databases)
            .field("fallback", &self.fallback)
            .finish()
    }
}

impl DatabaseCustomNode {
    pub fn new(databases: Arc<Databases>) -> Self {
        Self::with_fallback(databases, Box::new(NoopCustomNode))
    }

    pub fn with_fallback(databases: Arc<Databases>, fallback: FallbackAdapter) -> Self {
        Self {
            databases,
            fallback,
            templates: [
                Mutex::new(LruCache::new(TEMPLATE_CACHE_CAPACITY)),
                Mutex::new(LruCache::new(TEMPLATE_CACHE_CAPACITY)),
            ],
        }
    }

    pub fn databases(&self) -> &Arc<Databases> {
        &self.databases
    }

    fn template(&self, sql: &str, dialect: Dialect) -> Result<Arc<QueryTemplate>, String> {
        let cache = &self.templates[cache_slot(dialect)];
        if let Ok(mut cache) = cache.lock() {
            if let Some(template) = cache.get(sql) {
                return Ok(template.clone());
            }
        }
        let parsed = Arc::new(
            QueryTemplate::parse(sql, dialect)
                .map_err(|e| format!("Invalid database query template: {e}"))?,
        );
        if let Ok(mut cache) = cache.lock() {
            cache.insert(sql.to_string(), parsed.clone());
        }
        Ok(parsed)
    }

    #[cfg(test)]
    fn cached_templates(&self, dialect: Dialect) -> usize {
        self.templates[cache_slot(dialect)]
            .lock()
            .map_or(0, |cache| cache.len())
    }

    /// The message of each failure is the plain error text; the failure is what the trace entry
    /// shows (MS-FFI.md "Error trace").
    async fn run_database(&self, request: CustomNodeRequest) -> NodeResult {
        let node_id = &request.node.id;
        let config = parse_config(&request.node.config).map_err(|f| node_error(node_id, f))?;
        let sql = required_sql(&config).map_err(|f| node_error(node_id, f))?;
        let connection = self
            .resolve_connection(&config)
            .map_err(|f| node_error(node_id, f))?;
        let template = self
            .template_for(sql, &connection)
            .map_err(|f| node_error(node_id, f))?;
        let step = QueryStep {
            databases: &self.databases,
            sql,
            template: &template,
            label: NodeLabel {
                name: &request.node.name,
                id: &request.node.id,
            },
            connection: &connection,
        };

        let full_input = request.input.shallow_clone();
        let node_input =
            evaluate_input_field(&config, &full_input).map_err(|f| node_error(node_id, f))?;

        let pass_through = config.pass_through.unwrap_or(true);
        let default_handling = ResultHandling::default();
        let handling = config.result_handling.as_ref().unwrap_or(&default_handling);
        let (output, trace) = match config.execution_mode {
            ExecutionMode::Single => {
                let rows = step
                    .query_once(&node_input)
                    .await
                    .map_err(|f| node_error(node_id, f))?;
                shape(rows, handling, node_id)?
            }
            ExecutionMode::Loop => {
                let items = loop_items(&node_input, &config, &connection)
                    .map_err(|f| node_error(node_id, f))?;
                run_loop(step, items, handling, pass_through, node_id).await?
            }
        };

        let wrapped = Variable::empty_object();
        wrapped.dot_insert(config.output_path(), output);
        let output = if pass_through {
            strip_nodes(&full_input).merge_clone(&wrapped)
        } else {
            wrapped
        };
        Ok(NodeResponse {
            output,
            trace_data: Some(trace),
        })
    }

    fn resolve_connection(&self, config: &NodeConfig) -> Result<Arc<Connection>, Box<FailedNode>> {
        self.databases
            .resolve(config.instance.as_deref())
            .map_err(|e| config_failure(FailureKind::Evaluation, e.to_string(), e.to_string()))
    }

    fn template_for(
        &self,
        sql: &str,
        connection: &Connection,
    ) -> Result<Arc<QueryTemplate>, Box<FailedNode>> {
        let dialect = match connection.kind() {
            PoolKind::MySql => Dialect::MySql,
            PoolKind::Postgres => Dialect::Postgres,
        };
        self.template(sql, dialect).map_err(|message| {
            let failure = NodeFailure::new(FailureKind::Parse, message.clone())
                .with_part(FailurePart::Query)
                .with_expression(sql);
            Box::new(FailedNode::new(failure, message))
        })
    }
}

fn node_error(node_id: &Arc<str>, failed: Box<FailedNode>) -> NodeError {
    NodeError {
        node_id: node_id.clone(),
        trace: None,
        source: failed,
    }
}

fn config_failure(kind: FailureKind, problem: String, message: String) -> Box<FailedNode> {
    let failure = NodeFailure::new(kind, problem).with_part(FailurePart::Config);
    Box::new(FailedNode::new(failure, message))
}

/// Deserialises by reference: the config (editor leftovers included) is shared by every
/// evaluation of the node.
fn parse_config(config: &Value) -> Result<NodeConfig, Box<FailedNode>> {
    NodeConfig::deserialize(config).map_err(|e| {
        config_failure(
            FailureKind::Parse,
            e.to_string(),
            format!("Invalid database node config: {e}"),
        )
    })
}

fn required_sql(config: &NodeConfig) -> Result<&str, Box<FailedNode>> {
    config.sql().ok_or_else(|| {
        let message = "Database node has no query".to_string();
        config_failure(FailureKind::Parse, message.clone(), message)
    })
}

/// The value the query binds against: the full input, or what `inputField` evaluates to.
fn evaluate_input_field(
    config: &NodeConfig,
    full_input: &Variable,
) -> Result<Variable, Box<FailedNode>> {
    let Some(field) = config.input_field() else {
        return Ok(full_input.shallow_clone());
    };
    Isolate::with_environment(full_input.shallow_clone())
        .run_standard(field)
        .map_err(|e| {
            let failure =
                NodeFailure::expression(FailurePart::InputField, field, &e, full_input, None);
            Box::new(FailedNode::new(
                failure,
                format!("Failed to evaluate expression: {e}"),
            ))
        })
}

/// The items of a loop-mode node: its input must be an array of at most `maxLoopItems`.
fn loop_items(
    node_input: &Variable,
    config: &NodeConfig,
    connection: &Connection,
) -> Result<Vec<Variable>, Box<FailedNode>> {
    let items = node_input.as_array().ok_or_else(|| {
        let failure = NodeFailure::new(
            FailureKind::Type,
            format!("Expected an array, got {}", node_input.type_name()),
        )
        .with_part(FailurePart::InputField);
        let failure = match config.input_field() {
            Some(field) => failure.with_expression(field),
            None => failure,
        };
        Box::new(FailedNode::new(failure, "Expected an array".to_string()))
    })?;
    let items: Vec<Variable> = items.borrow().iter().map(Variable::shallow_clone).collect();
    if items.len() > connection.max_loop_items() {
        let error = connection.too_many_loop_items(items.len());
        let failure = NodeFailure::new(FailureKind::Evaluation, error.to_string())
            .with_part(FailurePart::InputField);
        return Err(Box::new(FailedNode::new(failure, error.to_string())));
    }
    Ok(items)
}

/// Applies `resultHandling`. A failure is a node error whose message is the analyst's text; the
/// structured part travels as the error's trace (and as `details` over the FFI).
fn shape(
    rows: Vec<Value>,
    handling: &ResultHandling,
    node_id: &Arc<str>,
) -> Result<(Variable, Variable), NodeError> {
    shape_result(rows, handling).map_err(|e| NodeError {
        node_id: node_id.clone(),
        trace: Some(Variable::from(e.details())),
        source: Box::new(e),
    })
}

/// Queries every item: `(output array, trace array)`, stopping at the first failure in item order.
async fn run_loop(
    step: QueryStep<'_>,
    items: Vec<Variable>,
    handling: &ResultHandling,
    pass_through: bool,
    node_id: &Arc<str>,
) -> Result<(Variable, Variable), NodeError> {
    let fetched = step.query_items(&items).await;
    let mut outputs = Vec::with_capacity(items.len());
    let mut traces = Vec::with_capacity(items.len());
    for (mut item, rows) in items.into_iter().zip(fetched) {
        let rows = rows.map(unshare).map_err(|f| node_error(node_id, f))?;
        let (result, trace) = shape(rows, handling, node_id)?;
        traces.push(trace);
        outputs.push(if pass_through {
            item.merge_clone(&result)
        } else {
            result
        });
    }
    Ok((Variable::from_array(outputs), Variable::from_array(traces)))
}

/// The node input without the engine-injected `$nodes` key.
fn strip_nodes(input: &Variable) -> Variable {
    match input {
        Variable::Object(object) => {
            let borrowed = object.borrow();
            if !borrowed.iter().any(|(key, _)| key.as_str() == "$nodes") {
                return input.shallow_clone();
            }
            let mut map = borrowed.clone();
            map.remove(&Variable::nodes_key());
            Variable::from_object(map)
        }
        other => other.shallow_clone(),
    }
}

impl CustomNodeAdapter for DatabaseCustomNode {
    fn handle(&self, request: CustomNodeRequest) -> Pin<Box<dyn Future<Output = NodeResult> + '_>> {
        if request.node.kind.as_ref() == DATABASE_NODE_KIND {
            Box::pin(self.run_database(request))
        } else {
            self.fallback.handle(request)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use serde_json::json;

    fn node() -> DatabaseCustomNode {
        DatabaseCustomNode::new(Arc::new(Databases::new(DatabaseConfig::default()).unwrap()))
    }

    #[test]
    fn result_handling_rejects_unknown_values_loudly() {
        let e = serde_json::from_value::<ResultHandling>(json!({"none": "stop"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("stop") && e.contains("continue"), "{e}");
        // null and absent both mean the default
        let cfg: NodeConfig =
            serde_json::from_value(json!({"query": "SELECT 1", "resultHandling": null})).unwrap();
        assert_eq!(
            cfg.result_handling.unwrap_or_default(),
            ResultHandling::default()
        );
    }

    #[test]
    fn template_cache_hits_share_one_parse() {
        let node = node();
        let a = node.template("SELECT {{a}}", Dialect::MySql).unwrap();
        let b = node.template("SELECT {{a}}", Dialect::MySql).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // same SQL, other dialect: a different template
        let c = node.template("SELECT {{a}}", Dialect::Postgres).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(c.scalar_sql().as_ref(), "SELECT $1");
    }

    #[test]
    fn template_cache_is_bounded() {
        let node = node();
        for i in 0..(TEMPLATE_CACHE_CAPACITY + 50) {
            node.template(&format!("SELECT {i}, {{{{a}}}}"), Dialect::MySql)
                .unwrap();
        }
        assert!(node.cached_templates(Dialect::MySql) <= TEMPLATE_CACHE_CAPACITY);
        // the most recent entry is still cached
        let sql = format!("SELECT {}, {{{{a}}}}", TEMPLATE_CACHE_CAPACITY + 49);
        let a = node.template(&sql, Dialect::MySql).unwrap();
        let b = node.template(&sql, Dialect::MySql).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
