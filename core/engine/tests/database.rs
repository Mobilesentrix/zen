use serde_json::{json, Value};
use std::fmt::Debug;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use zen_database::{
    DatabaseAdapter, DatabaseError, DatabaseRequest, DatabaseResponse, DatabaseResult,
};
use zen_engine::{Decision, DecisionEngine, EvaluationError, EvaluationOptions};

mod support;
use crate::support::load_test_data;

// ═══════════════════════════════════════════════════════════════════════════
//  Mock Adapters
// ═══════════════════════════════════════════════════════════════════════════

/// Returns hardcoded rows for any query.
#[derive(Debug, Clone)]
struct MockDatabaseAdapter {
    rows: Value,
}

impl MockDatabaseAdapter {
    fn with_rows(rows: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            rows: Value::Array(rows),
        })
    }

    fn empty() -> Arc<Self> {
        Self::with_rows(vec![])
    }
}

impl DatabaseAdapter for MockDatabaseAdapter {
    fn execute<'a>(
        &'a self,
        _request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        let rows = self.rows.clone();
        let row_count = match &rows {
            Value::Array(arr) => arr.len(),
            _ => 0,
        };
        Box::pin(async move {
            Ok(DatabaseResponse { rows, row_count })
        })
    }
}

/// Always returns a query error.
#[derive(Debug)]
struct FailingDatabaseAdapter {
    message: String,
}

impl FailingDatabaseAdapter {
    fn new(msg: &str) -> Arc<Self> {
        Arc::new(Self {
            message: msg.to_string(),
        })
    }
}

impl DatabaseAdapter for FailingDatabaseAdapter {
    fn execute<'a>(
        &'a self,
        _request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        let msg = self.message.clone();
        Box::pin(async move { Err(DatabaseError::Query(msg)) })
    }
}

/// Captures the query string for assertion.
#[derive(Debug)]
struct SpyDatabaseAdapter {
    captured_query: std::sync::Mutex<Option<String>>,
    rows: Value,
}

impl SpyDatabaseAdapter {
    fn new(rows: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            captured_query: std::sync::Mutex::new(None),
            rows: Value::Array(rows),
        })
    }

    fn captured_query(&self) -> Option<String> {
        self.captured_query.lock().unwrap().clone()
    }
}

impl DatabaseAdapter for SpyDatabaseAdapter {
    fn execute<'a>(
        &'a self,
        request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        // Capture the query
        *self.captured_query.lock().unwrap() = Some(request.query.clone());
        let rows = self.rows.clone();
        let row_count = match &rows {
            Value::Array(arr) => arr.len(),
            _ => 0,
        };
        Box::pin(async move {
            Ok(DatabaseResponse { rows, row_count })
        })
    }
}

/// Counts how many times execute() is called.
#[derive(Debug)]
struct CountingDatabaseAdapter {
    call_count: AtomicUsize,
    rows: Value,
}

impl CountingDatabaseAdapter {
    fn new(rows: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            call_count: AtomicUsize::new(0),
            rows: Value::Array(rows),
        })
    }

    fn count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl DatabaseAdapter for CountingDatabaseAdapter {
    fn execute<'a>(
        &'a self,
        _request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let rows = self.rows.clone();
        let row_count = match &rows {
            Value::Array(arr) => arr.len(),
            _ => 0,
        };
        Box::pin(async move {
            Ok(DatabaseResponse { rows, row_count })
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 1: Basic Happy Path
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_returns_rows() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"id": 1, "name": "Alice"}),
        json!({"id": 2, "name": "Bob"}),
    ]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    assert_eq!(
        result.result,
        json!({
            "rows": [
                {"id": 1, "name": "Alice"},
                {"id": 2, "name": "Bob"}
            ],
            "rowsCount": 2
        })
        .into()
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_empty_result() {
    let mock = MockDatabaseAdapter::empty();

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    assert_eq!(
        result.result,
        json!({
            "rows": [],
            "rowsCount": 0
        })
        .into()
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_single_row() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"count": 42}),
    ]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    assert_eq!(
        result.result,
        json!({
            "rows": [{"count": 42}],
            "rowsCount": 1
        })
        .into()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 2: Error Handling
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_query_error_propagates() {
    let mock = FailingDatabaseAdapter::new("connection refused");

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("connection refused"),
        "Expected 'connection refused', got: {}",
        err
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_error_contains_node_id() {
    let mock = FailingDatabaseAdapter::new("timeout");

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await;

    match result.unwrap_err().deref() {
        EvaluationError::NodeError { node_id, .. } => {
            assert_eq!(node_id.deref(), "db-1");
        }
        other => panic!("Expected NodeError, got: {:?}", other),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_noop_adapter_fails() {
    // Default Decision has NoopDatabaseAdapter → should fail with NotConfigured
    let content = load_test_data("database-node.json");
    let decision = Decision::from(content); // No .with_database()

    let result = decision.evaluate(json!({}).into()).await;

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not configured") || err_msg.contains("Database"),
        "Expected NotConfigured error, got: {}",
        err_msg
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 3: Trace / Observability
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_trace_enabled() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"id": 1, "name": "Alice"}),
    ]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);

    let result = decision
        .evaluate_with_opts(
            json!({}).into(),
            EvaluationOptions {
                trace: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // 3 nodes: input, database, output
    let trace = result.trace.unwrap();
    assert_eq!(trace.len(), 3);
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_trace_disabled() {
    let mock = MockDatabaseAdapter::with_rows(vec![json!({"id": 1})]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);

    let result = decision.evaluate(json!({}).into()).await.unwrap();

    assert!(result.trace.is_none());
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_trace_contains_output() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"id": 1, "name": "Alice"}),
    ]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);

    let result = decision
        .evaluate_with_opts(
            json!({}).into(),
            EvaluationOptions {
                trace: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let trace = result.trace.unwrap();
    // The database node's trace should have the output stored
    let db_trace = trace.get("db-1").expect("trace for db-1 should exist");
    let trace_output = db_trace.output.to_value();
    assert!(trace_output.get("rows").is_some());
    assert!(trace_output.get("rowsCount").is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 4: Query Passthrough
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_passes_correct_query() {
    let spy = SpyDatabaseAdapter::new(vec![]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(spy.clone());
    let _ = decision.evaluate(json!({}).into()).await;

    assert_eq!(
        spy.captured_query().unwrap(),
        "SELECT * FROM orders LIMIT 10"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 5: Transform Attributes (outputPath)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_output_path() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"sku": "ABC", "price": 9.99}),
    ]);

    let content = load_test_data("database-node-transform.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    // With outputPath="dbResult", the result should be nested
    let output = result.result.to_value();
    assert!(
        output.get("dbResult").is_some(),
        "Expected output to be nested under 'dbResult', got: {:?}",
        output
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 6: Engine / Decision Builders
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_via_engine_builder() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"product": "Widget", "price": 9.99}),
    ]);

    let engine = DecisionEngine::default().with_database(mock);
    let content = load_test_data("database-node.json");
    let decision = engine.create_decision(content.into());

    let result = decision.evaluate(json!({}).into()).await.unwrap();

    assert_eq!(
        result.result,
        json!({
            "rows": [{"product": "Widget", "price": 9.99}],
            "rowsCount": 1
        })
        .into()
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_decision_builder_chain() {
    let mock = MockDatabaseAdapter::with_rows(vec![json!({"x": 1})]);

    let content = load_test_data("database-node.json");
    // Ensure the builder chain works: from → with_database
    let decision = Decision::from(content).with_database(mock);

    assert!(decision.evaluate(json!({}).into()).await.is_ok());
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 7: Various Data Types in Rows
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_handles_null_values() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"id": 1, "email": null}),
    ]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    let output = result.result.to_value();
    let rows = output["rows"].as_array().unwrap();
    assert_eq!(rows[0]["email"], Value::Null);
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_handles_nested_json() {
    let mock = MockDatabaseAdapter::with_rows(vec![
        json!({"id": 1, "metadata": {"role": "admin", "perms": ["read", "write"]}}),
    ]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    let output = result.result.to_value();
    let rows = output["rows"].as_array().unwrap();
    assert_eq!(rows[0]["metadata"]["role"], "admin");
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_handles_large_result_set() {
    let rows: Vec<Value> = (0..1000)
        .map(|i| json!({"id": i, "value": format!("row_{}", i)}))
        .collect();
    let mock = MockDatabaseAdapter::with_rows(rows);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(mock);
    let result = decision.evaluate(json!({}).into()).await.unwrap();

    let output = result.result.to_value();
    assert_eq!(output["rowsCount"], 1000);
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 8: Serde / Deserialization
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn database_node_json_deserializes() {
    // This validates that the DatabaseNode variant roundtrips correctly
    let content = load_test_data("database-node.json");

    // Find the database node
    let db_node = content
        .nodes
        .iter()
        .find(|n| n.id.deref() == "db-1")
        .expect("db-1 node must exist");

    assert!(
        matches!(
            db_node.kind,
            zen_engine::model::DecisionNodeKind::DatabaseNode { .. }
        ),
        "Expected DatabaseNode variant"
    );

    // Roundtrip
    let serialized = serde_json::to_string(&content).unwrap();
    let deserialized: zen_engine::model::DecisionContent =
        serde_json::from_str(&serialized).unwrap();

    assert_eq!(content.nodes.len(), deserialized.nodes.len());
}

// ═══════════════════════════════════════════════════════════════════════════
//  TEST GROUP 9: Concurrency / Multiple Evaluations
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn database_node_multiple_evaluations() {
    let counter = CountingDatabaseAdapter::new(vec![json!({"id": 1})]);

    let content = load_test_data("database-node.json");
    let decision = Decision::from(content).with_database(counter.clone());

    for _ in 0..10 {
        let result = decision.evaluate(json!({}).into()).await;
        assert!(result.is_ok());
    }

    assert_eq!(counter.count(), 10);
}
