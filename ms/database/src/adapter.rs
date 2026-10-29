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

use crate::bind::{bind, BindError, BoundQuery, NodeLabel};
use crate::pool::Databases;
use crate::scope;
use crate::template::{Dialect, QueryTemplate};
use futures_util::stream::{self, StreamExt};
use hashlink::LruCache;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest, NoopCustomNode};
use zen_engine::nodes::failure::{
    missing_reads, FailedNode, FailureKind, FailurePart, NodeFailure,
};
use zen_engine::nodes::{NodeError, NodeResponse, NodeResult};
use zen_expression::{Isolate, Variable};

pub const DATABASE_NODE_KIND: &str = "database";

/// Parsed templates kept per dialect (LRU). Production rule sets are far smaller; the bound
/// matters for simulate/editor flows that evaluate ad-hoc SQL for the life of the engine.
pub(crate) const TEMPLATE_CACHE_CAPACITY: usize = 1024;

pub type FallbackAdapter = Box<dyn CustomNodeAdapter + Send + Sync>;

/// Queries one loop-mode node keeps in flight at once (the connection's pool still caps the
/// total across evaluations).
pub const LOOP_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionMode {
    #[default]
    Single,
    Loop,
}

/// Rows a result-handling error keeps in its details (the offending rows for "several").
pub const DETAIL_ROWS: usize = 10;

/// What a node does with the rows its query returned (see MS-FFI.md §3). The
/// defaults reproduce the plain `{rows, rowsCount}` output byte for byte.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResultHandling {
    #[serde(default)]
    pub none: NoneHandling,
    /// Message of the node error when `none` is `error` (default "Not found").
    #[serde(default)]
    pub none_message: Option<String>,
    #[serde(default)]
    pub one: OneHandling,
    /// `many` is accepted as an alias.
    #[serde(default, alias = "many")]
    pub several: SeveralHandling,
    /// Message of the node error when `several` is `error` (default "Duplicates found").
    #[serde(default, alias = "manyMessage")]
    pub several_message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoneHandling {
    /// Default: `{rows: [], rowsCount: 0}`.
    #[default]
    Continue,
    /// Fail the node with `noneMessage`.
    Error,
    /// Write `{rowsCount: 0}` (every column reads as null downstream).
    Empty,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OneHandling {
    /// Default: `{rows: [row], rowsCount: 1}`.
    #[default]
    All,
    /// Write the row object itself plus `rowsCount: 1`.
    Use,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeveralHandling {
    /// Default: `{rows: [...], rowsCount: n}`.
    #[default]
    All,
    /// Fail the node with `severalMessage`.
    Error,
    /// Write row 0 as an object plus `rowsCount: n`.
    First,
}

/// A node error raised by `resultHandling`: `Display` is the analyst's message, `details()` the
/// structured part (`reason`, `rowsCount`, and for "several" up to [`DETAIL_ROWS`] rows).
#[derive(Debug, Clone, PartialEq)]
pub struct ResultHandlingError {
    message: String,
    reason: &'static str,
    rows_count: usize,
    rows: Vec<Value>,
}

impl ResultHandlingError {
    pub fn none(message: String) -> Self {
        Self {
            message,
            reason: "none",
            rows_count: 0,
            rows: Vec::new(),
        }
    }

    pub fn several(message: String, rows_count: usize, mut rows: Vec<Value>) -> Self {
        rows.truncate(DETAIL_ROWS);
        Self {
            message,
            reason: "several",
            rows_count,
            rows,
        }
    }

    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn rows_count(&self) -> usize {
        self.rows_count
    }

    pub fn details(&self) -> Value {
        let mut details = Map::with_capacity(3);
        details.insert("reason".to_string(), Value::from(self.reason));
        details.insert("rowsCount".to_string(), Value::from(self.rows_count));
        if self.reason == "several" {
            details.insert("rows".to_string(), Value::Array(self.rows.clone()));
        }
        Value::Object(details)
    }
}

impl fmt::Display for ResultHandlingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ResultHandlingError {}

fn message_or(configured: &Option<String>, default: &str) -> String {
    match configured.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => default.to_string(),
    }
}

/// The plain output object, `{rows, rowsCount}`.
fn raw_result(rows: Vec<Value>) -> Variable {
    let count = rows.len();
    let mut result = Map::with_capacity(2);
    result.insert("rows".to_string(), Value::Array(rows));
    result.insert("rowsCount".to_string(), Value::from(count));
    Variable::from(Value::Object(result))
}

/// One row as the output object, with `rowsCount` added (it overwrites a column of that name).
fn row_with_count(mut row: Value, count: usize) -> Variable {
    if let Value::Object(object) = &mut row {
        object.insert("rowsCount".to_string(), Value::from(count));
    }
    Variable::from(row)
}

/// Applies `resultHandling` to the fetched rows: `(output, trace)`. The trace is always the raw
/// `{rows, rowsCount}`; with the defaults the output is that same object (shared).
pub(crate) fn shape_result(
    rows: Vec<Value>,
    handling: &ResultHandling,
) -> Result<(Variable, Variable), ResultHandlingError> {
    let count = rows.len();
    let shaped = match count {
        0 => match handling.none {
            NoneHandling::Continue => None,
            NoneHandling::Error => {
                return Err(ResultHandlingError::none(message_or(
                    &handling.none_message,
                    "Not found",
                )));
            }
            NoneHandling::Empty => Some(row_with_count(Value::Object(Map::new()), 0)),
        },
        1 => match handling.one {
            OneHandling::All => None,
            OneHandling::Use => Some(row_with_count(rows[0].clone(), 1)),
        },
        _ => match handling.several {
            SeveralHandling::All => None,
            SeveralHandling::Error => {
                return Err(ResultHandlingError::several(
                    message_or(&handling.several_message, "Duplicates found"),
                    count,
                    rows,
                ));
            }
            SeveralHandling::First => Some(row_with_count(rows[0].clone(), count)),
        },
    };
    let trace = raw_result(rows);
    Ok(match shaped {
        Some(output) => (output, trace),
        None => (trace.shallow_clone(), trace),
    })
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
        let q = self.query.as_deref().filter(|q| !q.trim().is_empty());
        q.or_else(|| {
            self.use_custom_query
                .then_some(self.custom_query.as_deref())
                .flatten()
                .filter(|q| !q.trim().is_empty())
        })
    }

    /// Same defaults module-go's TransformDatabaseNodes applies.
    fn output_path(&self) -> Option<&str> {
        match self.output_path.as_deref() {
            None => Some("dbResult"),
            Some("") => Some("dbResult"),
            Some(p) => Some(p),
        }
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

    pub(crate) fn template(
        &self,
        sql: &str,
        dialect: Dialect,
    ) -> Result<Arc<QueryTemplate>, String> {
        let cache = &self.templates[cache_slot(dialect)];
        if let Ok(mut cache) = cache.lock() {
            if let Some(t) = cache.get(sql) {
                return Ok(t.clone());
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
    pub(crate) fn cached_templates(&self, dialect: Dialect) -> usize {
        self.templates[cache_slot(dialect)]
            .lock()
            .map(|c| c.len())
            .unwrap_or(0)
    }

    async fn run_database(&self, request: CustomNodeRequest) -> NodeResult {
        let node_id = request.node.id.clone();
        // The message is the plain error text; the failure is what the trace entry shows
        // (MS-FFI.md "Error trace"). Only built when the node fails.
        let failed = |failed: Box<FailedNode>| NodeError {
            node_id: node_id.clone(),
            trace: None,
            source: failed,
        };
        let fail =
            |failure: NodeFailure, msg: String| failed(Box::new(FailedNode::new(failure, msg)));
        let config_failure = |kind: FailureKind, message: String| {
            NodeFailure::new(kind, message).with_part(FailurePart::Config)
        };

        // Deserialise by reference: the config (incl. editor leftovers such as
        // `selectedTables`) is shared by every evaluation of the node.
        let config = NodeConfig::deserialize(&*request.node.config).map_err(|e| {
            fail(
                config_failure(FailureKind::Parse, e.to_string()),
                format!("Invalid database node config: {e}"),
            )
        })?;
        let sql = config.sql().ok_or_else(|| {
            let message = "Database node has no query".to_string();
            fail(config_failure(FailureKind::Parse, message.clone()), message)
        })?;
        let conn = self
            .databases
            .resolve(config.instance.as_deref())
            .map_err(|e| {
                fail(
                    config_failure(FailureKind::Evaluation, e.to_string()),
                    e.to_string(),
                )
            })?;
        let dialect = match conn.kind() {
            crate::pool::PoolKind::MySql => Dialect::MySql,
            crate::pool::PoolKind::Postgres => Dialect::Postgres,
        };
        let template = self.template(sql, dialect).map_err(|message| {
            let failure = NodeFailure::new(FailureKind::Parse, message.clone())
                .with_part(FailurePart::Query)
                .with_expression(sql);
            fail(failure, message)
        })?;
        let label = NodeLabel {
            name: &request.node.name,
            id: &request.node.id,
        };

        let full_input = request.input.shallow_clone();
        let node_input = match config.input_field.as_deref().filter(|f| !f.is_empty()) {
            None => full_input.shallow_clone(),
            Some(field) => Isolate::with_environment(full_input.shallow_clone())
                .run_standard(field)
                .map_err(|e| {
                    let failure = NodeFailure::expression(
                        FailurePart::InputField,
                        field,
                        &e,
                        &full_input,
                        None,
                    );
                    fail(failure, format!("Failed to evaluate expression: {e}"))
                })?,
        };

        let pass_through = config.pass_through.unwrap_or(true);
        let handling = config.result_handling.clone().unwrap_or_default();
        // A result-handling failure is a node error whose message is the analyst's text; the
        // structured part travels as the error's trace (and as `details` over the FFI).
        let shape = |rows: Vec<Value>| {
            shape_result(rows, &handling).map_err(|e| NodeError {
                node_id: node_id.clone(),
                trace: Some(Variable::from(e.details())),
                source: Box::new(e),
            })
        };
        let mut trace_items = Vec::new();
        let mut output = match config.execution_mode {
            ExecutionMode::Single => {
                let rows = self
                    .query_once(sql, &template, &node_input, label, &conn)
                    .await
                    .map_err(failed)?;
                let (result, trace) = shape(rows)?;
                trace_items.push(trace);
                result
            }
            ExecutionMode::Loop => {
                let items = node_input.as_array().ok_or_else(|| {
                    let failure = NodeFailure::new(
                        FailureKind::Type,
                        format!("Expected an array, got {}", node_input.type_name()),
                    )
                    .with_part(FailurePart::InputField);
                    let failure = match config.input_field.as_deref().filter(|f| !f.is_empty()) {
                        Some(field) => failure.with_expression(field),
                        None => failure,
                    };
                    fail(failure, "Expected an array".to_string())
                })?;
                let items: Vec<Variable> =
                    items.borrow().iter().map(Variable::shallow_clone).collect();
                if items.len() > conn.max_loop_items() {
                    let e = conn.too_many_loop_items(items.len());
                    let failure = NodeFailure::new(FailureKind::Evaluation, e.to_string())
                        .with_part(FailurePart::InputField);
                    return Err(fail(failure, e.to_string()));
                }
                let fetched = self.query_items(sql, &template, &items, label, &conn).await;
                let mut out = Vec::with_capacity(items.len());
                for (mut item, rows) in items.into_iter().zip(fetched) {
                    let rows = rows.map(unshare).map_err(failed)?;
                    let (result, trace) = shape(rows)?;
                    trace_items.push(trace);
                    out.push(if pass_through {
                        item.merge_clone(&result)
                    } else {
                        result
                    });
                }
                Variable::from_array(out)
            }
        };

        if let Some(path) = config.output_path() {
            let wrapped = Variable::empty_object();
            wrapped.dot_insert(path, output);
            output = wrapped;
        }
        if pass_through {
            output = strip_nodes(&full_input).merge_clone(&output);
        }

        let trace = match config.execution_mode {
            ExecutionMode::Single => trace_items.pop(),
            ExecutionMode::Loop => Some(Variable::from_array(trace_items)),
        };
        Ok(NodeResponse {
            output,
            trace_data: trace,
        })
    }

    /// Binds and runs the query once; the rows as decoded JSON objects.
    async fn query_once(
        &self,
        sql: &str,
        template: &QueryTemplate,
        context: &Variable,
        label: NodeLabel<'_>,
        conn: &Arc<crate::pool::Connection>,
    ) -> Result<Vec<Value>, Box<FailedNode>> {
        let bound = bind(template, context, label).map_err(|e| bind_failure(e, context))?;
        self.execute_bound(sql, template, bound, conn)
            .await
            .map(unshare)
    }

    /// Loop mode: one query per item. The result per item is in item order and the caller
    /// stops at the first failure in that order. A read-only statement runs up to
    /// [`LOOP_CONCURRENCY`] queries at once and sends identical queries once: the error
    /// reported is still the one a one-at-a-time loop would report (items after a failed bind
    /// are never queried; a query whose item comes after a failure is only wasted read work).
    /// Any other statement runs one item after another and stops at the first failure.
    async fn query_items(
        &self,
        sql: &str,
        template: &QueryTemplate,
        items: &[Variable],
        label: NodeLabel<'_>,
        conn: &Arc<crate::pool::Connection>,
    ) -> Vec<Result<Rows, Box<FailedNode>>> {
        if !template.is_read_only() {
            let mut results = Vec::with_capacity(items.len());
            for item in items {
                let result = match bind(template, item, label) {
                    Ok(bound) => self.execute_bound(sql, template, bound, conn).await,
                    Err(e) => Err(bind_failure(e, item)),
                };
                let failed = result.is_err();
                results.push(result);
                if failed {
                    break;
                }
            }
            return results;
        }
        let mut bound = Vec::with_capacity(items.len());
        for item in items {
            match bind(template, item, label) {
                Ok(b) => bound.push(Ok(b)),
                Err(e) => {
                    bound.push(Err(bind_failure(e, item)));
                    break;
                }
            }
        }
        // item index -> index of the query that answers it
        let mut unique: Vec<BoundQuery> = Vec::new();
        let mut answer: Vec<Option<usize>> = Vec::with_capacity(bound.len());
        let mut seen: HashMap<scope::DedupKey, usize> = HashMap::new();
        for b in &bound {
            answer.push(b.as_ref().ok().map(|query| {
                if conn.dedupe() {
                    *seen
                        .entry(scope::DedupKey::new(conn.name(), query))
                        .or_insert_with(|| {
                            unique.push(query.clone());
                            unique.len() - 1
                        })
                } else {
                    unique.push(query.clone());
                    unique.len() - 1
                }
            }));
        }
        let rows: Vec<Result<Rows, Box<FailedNode>>> = stream::iter(
            unique
                .into_iter()
                .map(|query| self.execute_bound(sql, template, query, conn)),
        )
        .buffered(LOOP_CONCURRENCY)
        .collect()
        .await;
        // Hand each query's rows to the items it answers, in item order; the list ends at the
        // first failure (the caller stops there, so nothing after it is needed).
        let mut rows: Vec<Result<Rows, Option<Box<FailedNode>>>> =
            rows.into_iter().map(|r| r.map_err(Some)).collect();
        let mut results = Vec::with_capacity(bound.len());
        for (b, slot) in bound.into_iter().zip(answer) {
            let result = match (b, slot) {
                (Err(failure), _) => Err(failure),
                (Ok(_), Some(k)) => match &mut rows[k] {
                    Ok(r) => Ok(r.clone()),
                    Err(failure) => match failure.take() {
                        Some(failure) => Err(failure),
                        // an earlier item with this query failed; the list already ended
                        None => break,
                    },
                },
                (Ok(_), None) => unreachable!("every bound item has a query"),
            };
            let failed = result.is_err();
            results.push(result);
            if failed {
                break;
            }
        }
        results
    }

    /// Runs a bound query. Inside one evaluation scope a read-only query identical to one
    /// already run on the same connection is answered from those rows (never from a previous
    /// call); any other statement always goes to the database.
    async fn execute_bound(
        &self,
        sql: &str,
        template: &QueryTemplate,
        bound: BoundQuery,
        conn: &Arc<crate::pool::Connection>,
    ) -> Result<Rows, Box<FailedNode>> {
        let key = (conn.dedupe() && template.is_read_only())
            .then(|| scope::DedupKey::new(conn.name(), &bound));
        if let Some(rows) = key.as_ref().and_then(scope::lookup) {
            return Ok(rows);
        }
        let rows = self.databases.execute(conn, bound).await.map_err(|e| {
            let failure = NodeFailure::new(FailureKind::Evaluation, e.to_string())
                .with_part(FailurePart::Query)
                .with_expression(sql);
            Box::new(FailedNode::new(
                failure,
                format!("Database query execution failed: {e}"),
            ))
        })?;
        let rows = Rc::new(rows);
        if let Some(key) = key {
            scope::store(key, rows.clone());
        }
        Ok(rows)
    }
}

/// Rows of one query, shared between the items (and the evaluation's dedupe table) they answer.
type Rows = Rc<Vec<Value>>;

/// The rows for one consumer: moved out when nobody else holds them, copied otherwise.
fn unshare(rows: Rows) -> Vec<Value> {
    Rc::try_unwrap(rows).unwrap_or_else(|shared| shared.as_ref().clone())
}

/// A placeholder that could not be bound: the placeholder, its kind and, for a null, the paths
/// it reads that are absent or null in the input.
fn bind_failure(error: BindError, context: &Variable) -> Box<FailedNode> {
    let message = error.to_string();
    let failure = match &error {
        BindError::Placeholder {
            expression, kind, ..
        } => {
            let missing = match kind {
                FailureKind::MissingValue => missing_reads(expression, false, context, None),
                _ => Vec::new(),
            };
            NodeFailure::new(*kind, message.clone())
                .with_part(FailurePart::Placeholder)
                .with_expression(expression.as_str())
                .with_missing(missing)
        }
        BindError::TooManyParams { .. } => {
            NodeFailure::new(FailureKind::Evaluation, message.clone()).with_part(FailurePart::Query)
        }
    };
    Box::new(FailedNode::new(failure, message))
}

/// The node input without the engine-injected `$nodes` key.
fn strip_nodes(input: &Variable) -> Variable {
    match input {
        Variable::Object(obj) => {
            let borrowed = obj.borrow();
            if !borrowed.iter().any(|(k, _)| k.as_str() == "$nodes") {
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

    fn rows(n: usize) -> Vec<Value> {
        (1..=n)
            .map(|i| json!({"id": i, "sku": format!("S{i}")}))
            .collect()
    }

    fn handling(v: Value) -> ResultHandling {
        serde_json::from_value(v).unwrap()
    }

    fn shaped(rows: Vec<Value>, h: &ResultHandling) -> Result<(Value, Value), ResultHandlingError> {
        shape_result(rows, h).map(|(out, trace)| {
            (
                serde_json::to_value(&out).unwrap(),
                serde_json::to_value(&trace).unwrap(),
            )
        })
    }

    #[test]
    fn result_handling_default_keeps_todays_shape() {
        let h = ResultHandling::default();
        assert_eq!(h, handling(json!({})));
        assert_eq!(
            h,
            handling(json!({"none": "continue", "one": "all", "several": "all"}))
        );
        for n in [0, 1, 3] {
            let (out, trace) = shaped(rows(n), &h).unwrap();
            assert_eq!(out, json!({"rows": rows(n), "rowsCount": n}));
            assert_eq!(trace, out);
        }
    }

    #[test]
    fn result_handling_none() {
        let (out, _) = shaped(rows(0), &handling(json!({"none": "empty"}))).unwrap();
        assert_eq!(out, json!({"rowsCount": 0}));
        let e = shaped(rows(0), &handling(json!({"none": "error"}))).unwrap_err();
        assert_eq!(e.to_string(), "Not found");
        assert_eq!(e.details(), json!({"reason": "none", "rowsCount": 0}));
        let e = shaped(
            rows(0),
            &handling(json!({"none": "error", "noneMessage": "No such product"})),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "No such product");
        // blank message falls back to the default
        let e = shaped(
            rows(0),
            &handling(json!({"none": "error", "noneMessage": "  "})),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "Not found");
        // none-handling does not touch one/several
        let (out, _) = shaped(rows(1), &handling(json!({"none": "error"}))).unwrap();
        assert_eq!(out, json!({"rows": rows(1), "rowsCount": 1}));
    }

    #[test]
    fn result_handling_one_use_writes_the_row_object() {
        let h = handling(json!({"one": "use"}));
        let (out, trace) = shaped(rows(1), &h).unwrap();
        assert_eq!(out, json!({"id": 1, "sku": "S1", "rowsCount": 1}));
        assert_eq!(
            trace,
            json!({"rows": rows(1), "rowsCount": 1}),
            "trace keeps the raw result"
        );
        // a column literally named rowsCount is overwritten by the count (documented)
        let (out, _) = shape_result(vec![json!({"rowsCount": "x", "a": 1})], &h).unwrap();
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({"rowsCount": 1, "a": 1})
        );
        // zero or several rows are unaffected by `one`
        let (out, _) = shaped(rows(2), &h).unwrap();
        assert_eq!(out, json!({"rows": rows(2), "rowsCount": 2}));
    }

    #[test]
    fn result_handling_several() {
        let (out, _) = shaped(rows(3), &handling(json!({"several": "first"}))).unwrap();
        assert_eq!(out, json!({"id": 1, "sku": "S1", "rowsCount": 3}));
        let e = shaped(rows(3), &handling(json!({"several": "error"}))).unwrap_err();
        assert_eq!(e.to_string(), "Duplicates found");
        assert_eq!(
            e.details(),
            json!({"reason": "several", "rowsCount": 3, "rows": rows(3)})
        );
        let e = shaped(
            rows(3),
            &handling(json!({"several": "error", "severalMessage": "More than one match"})),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "More than one match");
        // the offending rows in the details are capped
        let e = shaped(
            rows(DETAIL_ROWS + 5),
            &handling(json!({"several": "error"})),
        )
        .unwrap_err();
        assert_eq!(e.details()["rows"].as_array().unwrap().len(), DETAIL_ROWS);
        assert_eq!(e.details()["rowsCount"], json!(DETAIL_ROWS + 5));
        // `many` / `manyMessage` are accepted as aliases
        assert_eq!(
            handling(json!({"many": "first", "manyMessage": "m"})),
            handling(json!({"several": "first", "severalMessage": "m"}))
        );
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
