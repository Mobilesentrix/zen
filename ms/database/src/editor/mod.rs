//! The rule editor's Lookup step (node type `databaseNode`) turned into the engine's database
//! custom node, for every embedder of the engine (the uniffi binding behind the Java module, Rust
//! services). Ported from the Go module (`rulestore.TransformDatabaseNodesRouted` and
//! `PrepareContent`), which the Rules Engine backend runs, with its tests: the same rule
//! produces the same SQL and the same node config in every module.
//!
//! ```json
//! {"type": "databaseNode", "content": {"instance": "<id>", "useCustomQuery": true, "customQuery": "SELECT …"}}
//! ```
//! becomes
//! ```json
//! {"type": "customNode", "content": {"kind": "database",
//!   "config": {"query": "SELECT …", "outputPath": "dbResult", "passThrough": true, "editorInstance": "<id>"}}}
//! ```

mod literal;
mod query;
mod where_clause;

pub use where_clause::build_where_clause;

use crate::adapter::DATABASE_NODE_KIND;
use serde::Deserialize;
use serde_json::{Map, Value};
use zen_engine::model::DecisionContent;

/// The node type the rule editor saves for a Lookup step.
pub const EDITOR_DATABASE_NODE_TYPE: &str = "databaseNode";

/// Holds the editor's instance id when steps are not routed by instance; the engine ignores it.
const EDITOR_INSTANCE_KEY: &str = "editorInstance";

/// Selects the connection (id or alias) of the connection map that runs the step.
const ENGINE_INSTANCE_KEY: &str = "instance";

const DEFAULT_OUTPUT_PATH: &str = "dbResult";

/// The editor keys the query is compiled from, and the instance (stored under the routing key):
/// they are not forwarded into the node config.
const COMPILED_KEYS: &[&str] = &[
    "useCustomQuery",
    "customQuery",
    "selectedTables",
    "whereClauseGroup",
    ENGINE_INSTANCE_KEY,
    EDITOR_INSTANCE_KEY,
];

/// Which connection runs a Lookup step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstanceRouting {
    /// Every step runs on the default connection. The editor's instance is kept as
    /// `editorInstance`, which the engine ignores. (The Go module with `database.url`.)
    #[default]
    DefaultConnection,
    /// Each step runs on the connection, or alias, named by its editor instance; an instance that
    /// is not configured is a node error. (The Go module with `database.connections`.)
    ByInstance,
}

impl InstanceRouting {
    fn instance_key(self) -> &'static str {
        match self {
            Self::DefaultConnection => EDITOR_INSTANCE_KEY,
            Self::ByInstance => ENGINE_INSTANCE_KEY,
        }
    }
}

/// Why rule content cannot be prepared for the engine.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PrepareError {
    #[error("Invalid content JSON: {0}")]
    InvalidJson(String),
    /// The step's SQL cannot be built safely (unsupported operator, a non-number in a number
    /// column, …): the rule is refused rather than queried.
    #[error("failed to transform database nodes: database node \"{node_id}\": {message}")]
    DatabaseNode { node_id: String, message: String },
    #[error("Invalid decision content: {0}")]
    InvalidContent(String),
}

/// Whether the content has a Lookup step in the editor's form.
pub fn has_editor_database_node(content: &Value) -> bool {
    content
        .get("nodes")
        .and_then(Value::as_array)
        .is_some_and(|nodes| nodes.iter().any(is_editor_database_node))
}

fn is_editor_database_node(node: &Value) -> bool {
    node.get("type").and_then(Value::as_str) == Some(EDITOR_DATABASE_NODE_TYPE)
}

/// Rewrites every editor Lookup step of `content` (a decision's JSON) into the engine's database
/// custom node, in place. Content that cannot be compiled into a query yet (already converted,
/// or incomplete) is carried over as the config, so the node reports its own error only if an
/// evaluation reaches it. Other nodes are left untouched.
///
/// # Errors
/// A step's SQL cannot be built safely; the error names the node.
pub fn transform_database_nodes(
    content: &mut Value,
    routing: InstanceRouting,
) -> Result<(), PrepareError> {
    let Some(nodes) = content.get_mut("nodes").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for node in nodes
        .iter_mut()
        .filter(|node| is_editor_database_node(node))
    {
        if let Some(node) = node.as_object_mut() {
            transform_node(node, routing.instance_key())?;
        }
    }
    Ok(())
}

/// Decision JSON as stored by the Rules Engine, ready for the engine: Lookup steps converted.
///
/// # Errors
/// The bytes are not JSON, a Lookup step's SQL cannot be built safely, or the result is not a
/// valid decision.
pub fn prepare_decision_content(
    json: &[u8],
    routing: InstanceRouting,
) -> Result<DecisionContent, PrepareError> {
    let value: Value =
        serde_json::from_slice(json).map_err(|e| PrepareError::InvalidJson(e.to_string()))?;
    prepare_decision_value(value, routing)
}

/// [`prepare_decision_content`] for content already parsed.
///
/// # Errors
/// A Lookup step's SQL cannot be built safely, or the result is not a valid decision.
pub fn prepare_decision_value(
    mut value: Value,
    routing: InstanceRouting,
) -> Result<DecisionContent, PrepareError> {
    transform_database_nodes(&mut value, routing)?;
    DecisionContent::deserialize(value).map_err(|e| PrepareError::InvalidContent(e.to_string()))
}

fn transform_node(node: &mut Map<String, Value>, instance_key: &str) -> Result<(), PrepareError> {
    let content = match node.remove("content") {
        Some(Value::Object(content)) => content,
        _ => Map::new(),
    };
    let config = match build_config(&content, instance_key) {
        Ok(Some(config)) => config,
        Ok(None) => carry_over(content, instance_key),
        Err(message) => {
            return Err(PrepareError::DatabaseNode {
                node_id: node
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                message,
            })
        }
    };

    let mut custom = Map::new();
    custom.insert("kind".into(), Value::from(DATABASE_NODE_KIND));
    custom.insert("config".into(), Value::Object(config));
    node.insert("type".into(), Value::from("customNode"));
    node.insert("content".into(), Value::Object(custom));
    Ok(())
}

/// Content that holds no compilable query, with the editor instance moved under `instance_key`.
fn carry_over(content: Map<String, Value>, instance_key: &str) -> Map<String, Value> {
    content
        .into_iter()
        .map(|(key, value)| {
            if key == ENGINE_INSTANCE_KEY || key == EDITOR_INSTANCE_KEY {
                (instance_key.to_string(), value)
            } else {
                (key, value)
            }
        })
        .collect()
}

/// The `{query, outputPath, passThrough}` config, plus every editor option that is not compiled
/// into the query (the engine ignores keys it does not read). `Ok(None)` when the content has no
/// query source.
fn build_config(
    content: &Map<String, Value>,
    instance_key: &str,
) -> Result<Option<Map<String, Value>>, String> {
    let database = content
        .get("database")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(query) = query::compile_query(content, database)? else {
        return Ok(None);
    };

    let mut config: Map<String, Value> = content
        .iter()
        .filter(|(key, _)| !COMPILED_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    config.insert("query".into(), Value::from(query));
    config.insert("outputPath".into(), Value::from(output_path(content)));
    config.insert("passThrough".into(), Value::from(pass_through(content)));
    if let Some(instance) = content
        .get(ENGINE_INSTANCE_KEY)
        .and_then(Value::as_str)
        .filter(|instance| !instance.is_empty())
    {
        config.insert(instance_key.into(), Value::from(instance));
    }
    if !database.is_empty() {
        config.insert("database".into(), Value::from(database));
    }
    Ok(Some(config))
}

fn output_path(content: &Map<String, Value>) -> &str {
    content
        .get("outputPath")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .unwrap_or(DEFAULT_OUTPUT_PATH)
}

fn pass_through(content: &Map<String, Value>) -> bool {
    content
        .get("passThrough")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests;
