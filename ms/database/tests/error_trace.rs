//! MobileSentrix error trace for the database node: a traced evaluation error carries, on the
//! failing database step's trace entry, what failed (`error`, see MS-FFI.md "Error trace").
//! The first tests fail before any connection is opened (lazy pool on a closed port); the last
//! one needs the ms-test MySQL container (MS-BUILD.md "Integration tests").

use ms_zen_database::{DatabaseConfig, DatabaseCustomNode, Databases};
use serde_json::{json, Value};
use std::sync::Arc;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{DecisionEngine, EvaluationOptions, EvaluationTraceKind};

fn databases(url: &str) -> Arc<Databases> {
    let config: DatabaseConfig =
        serde_json::from_value(json!({"connections": {"db": {"url": url}}, "default": "db"}))
            .unwrap();
    Arc::new(Databases::new(config).unwrap())
}

fn graph(config: Value) -> DecisionContent {
    serde_json::from_value(json!({
        "nodes": [
            {"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}},
            {"id": "db", "name": "Lookup", "type": "customNode", "position": {"x": 0, "y": 0},
             "content": {"kind": "database", "config": config}},
            {"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}
        ],
        "edges": [
            {"id": "e1", "type": "edge", "sourceId": "in", "targetId": "db"},
            {"id": "e2", "type": "edge", "sourceId": "db", "targetId": "out"}
        ]
    }))
    .unwrap()
}

/// Evaluates on a current-thread runtime; the error JSON with its trace.
fn failure(dbs: Arc<Databases>, config: Value, input: Value, trace: bool) -> Value {
    let loader = MemoryLoader::default();
    loader.add("rule", graph(config));
    let engine = DecisionEngine::new(Arc::new(loader), Arc::new(DatabaseCustomNode::new(dbs)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime
        .block_on(engine.evaluate_with_opts(
            "rule",
            input.into(),
            EvaluationOptions {
                trace,
                max_depth: 5,
            },
        ))
        .expect_err("the evaluation fails");
    let mode = match trace {
        true => EvaluationTraceKind::Default,
        false => EvaluationTraceKind::None,
    };
    error
        .serialize_with_mode(serde_json::value::Serializer, mode)
        .unwrap()
}

const CLOSED: &str = "mysql://root:test@127.0.0.1:1/none";

#[test]
fn null_placeholder_is_a_missing_value_with_its_placeholder() {
    let query = "SELECT sku FROM products WHERE cost = {{ product.cost }}";
    let error = failure(
        databases(CLOSED),
        json!({"query": query}),
        json!({"product": {"sku": "1"}}),
        true,
    );
    assert_eq!(error["nodeId"], json!("db"));
    let source = error["source"].as_str().unwrap();
    assert!(source.contains("resolved to null"), "{error}");
    let entry = &error["trace"]["db"]["error"];
    assert_eq!(entry["kind"], json!("missingValue"), "{error}");
    assert_eq!(entry["part"], json!("placeholder"));
    assert_eq!(entry["expression"], json!("product.cost"));
    assert_eq!(entry["missing"], json!(["product.cost"]));
    assert_eq!(
        entry["message"],
        json!(source),
        "the message is the full placeholder text"
    );
    assert_eq!(error["trace"]["db"]["input"]["product"]["sku"], json!("1"));
}

#[test]
fn unbindable_placeholder_is_a_type_error_and_bad_template_a_parse_error() {
    let typed = failure(
        databases(CLOSED),
        json!({"query": "SELECT 1 WHERE x = {{ item }}"}),
        json!({"item": {"a": 1}}),
        true,
    );
    assert_eq!(
        typed["trace"]["db"]["error"]["kind"],
        json!("type"),
        "{typed}"
    );
    assert!(typed["trace"]["db"]["error"].get("missing").is_none());

    let template = "SELECT * FROM {{ t }}";
    let parse = failure(
        databases(CLOSED),
        json!({"query": template}),
        json!({"t": "x"}),
        true,
    );
    let entry = &parse["trace"]["db"]["error"];
    assert_eq!(entry["kind"], json!("parse"), "{parse}");
    assert_eq!(entry["part"], json!("query"));
    assert_eq!(entry["expression"], json!(template));
}

#[test]
fn untraced_database_error_is_unchanged() {
    let error = failure(
        databases(CLOSED),
        json!({"query": "SELECT {{ customer_id }}"}),
        json!({}),
        false,
    );
    let mut keys: Vec<&str> = error
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["nodeId", "source", "type"], "{error}");
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL)"]
fn failed_query_is_an_evaluation_error_with_its_sql() {
    let url = std::env::var("MS_TEST_MYSQL_URL").expect("MS_TEST_MYSQL_URL");
    let query = "SELECT no_such_column FROM no_such_table WHERE id = {{ id }}";
    let error = failure(
        databases(&url),
        json!({"query": query}),
        json!({"id": 1}),
        true,
    );
    let source = error["source"].as_str().unwrap();
    assert!(
        source.starts_with("Database query execution failed"),
        "{error}"
    );
    let entry = &error["trace"]["db"]["error"];
    assert_eq!(entry["kind"], json!("evaluation"));
    assert_eq!(entry["part"], json!("query"));
    assert_eq!(entry["expression"], json!(query));
    assert!(
        source.ends_with(entry["message"].as_str().unwrap()),
        "message is the database's own error: {error}"
    );
}

#[test]
fn a_loop_over_max_loop_items_fails_before_any_query() {
    let config: DatabaseConfig = serde_json::from_value(json!({
        "connections": {"db": {"url": CLOSED, "maxLoopItems": 2}}, "default": "db"
    }))
    .unwrap();
    let error = failure(
        Arc::new(Databases::new(config).unwrap()),
        json!({"query": "SELECT {{ id }} AS id", "executionMode": "loop", "inputField": "ids"}),
        json!({"ids": [1, 2, 3]}),
        true,
    );
    let source = error["source"].as_str().unwrap();
    assert!(
        source.contains("loop of 3 items exceeds maxLoopItems (2)"),
        "{error}"
    );
    assert_eq!(error["trace"]["db"]["error"]["part"], json!("inputField"));

    // the default is 1000 items; zero is rejected
    let zero: DatabaseConfig = serde_json::from_value(json!({
        "connections": {"db": {"url": CLOSED, "maxLoopItems": 0}}
    }))
    .unwrap();
    let e = Databases::new(zero).unwrap_err().to_string();
    assert!(e.contains("maxLoopItems must be at least 1"), "{e}");
    assert_eq!(ms_zen_database::DEFAULT_MAX_LOOP_ITEMS, 1000);
}
