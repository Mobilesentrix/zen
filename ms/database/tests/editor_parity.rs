//! The Rust conversion of the editor's Lookup step must equal the Go module's, which the Rules
//! Engine backend runs: same SQL, same node config, same refusals.
//!
//! `editor_parity/cases.jsonl` (built by `gen_cases.py`) holds ~3,000 Lookup steps covering
//! every builder path: each operator x value x value type, request-sourced values, nested
//! groups, several tables, custom queries, carry-over content, both routings, refusals.
//! `editor_parity/expected.jsonl` is the Go module's output for each case (ms-ruleengine-module-go
//! 71cc9c6, `rulestore.TransformDatabaseNodesRouted`), produced with:
//!
//! ```sh
//! docker run --rm -v "$PWD/ms/database/tests/editor_parity:/p" \
//!   -v "<ms-ruleengine-module-go>/internal/rulestore:/rs:ro" golang:1.25 \
//!   bash /p/go/run.sh /rs /p/cases.jsonl > ms/database/tests/editor_parity/expected.jsonl
//! ```

use ms_zen_database::{transform_database_nodes, InstanceRouting, PrepareError};
use serde_json::{json, Value};

const CASES: &str = include_str!("editor_parity/cases.jsonl");
const EXPECTED: &str = include_str!("editor_parity/expected.jsonl");

/// The Go module decodes rule JSON into float64s, so numbers it carries over unchanged are
/// re-encoded; compare numbers as doubles (the SQL text itself is compared exactly).
fn normalize(value: Value) -> Value {
    match value {
        Value::Number(number) => number.as_f64().map_or(Value::Null, Value::from),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

fn rule_of(case: &Value) -> Value {
    match case.get("rule") {
        Some(rule) => rule.clone(),
        None => json!({"nodes": [{"id": "db", "name": "Lookup", "type": "databaseNode",
                                  "content": case["content"]}], "edges": []}),
    }
}

#[test]
fn conversion_matches_the_go_module() {
    let cases: Vec<&str> = CASES.lines().collect();
    let expected: Vec<&str> = EXPECTED.lines().collect();
    assert_eq!(cases.len(), expected.len());
    assert!(cases.len() > 2_500, "corpus too small: {}", cases.len());

    let mut mismatches = Vec::new();
    for (index, (case, want)) in cases.iter().zip(&expected).enumerate() {
        let case: Value = serde_json::from_str(case).unwrap();
        let want: Value = serde_json::from_str(want).unwrap();
        let routing = if case["routing"] == "instance" {
            InstanceRouting::ByInstance
        } else {
            InstanceRouting::DefaultConnection
        };

        let mut rule = rule_of(&case);
        let got = match transform_database_nodes(&mut rule, routing) {
            Ok(()) => json!({ "rule": normalize(rule) }),
            Err(PrepareError::DatabaseNode { node_id, message }) => {
                json!({ "error": format!("database node {node_id:?}: {message}") })
            }
            Err(other) => json!({ "error": format!("unexpected: {other}") }),
        };
        let want = match want.get("rule") {
            Some(rule) => json!({ "rule": normalize(rule.clone()) }),
            None => want,
        };
        if got != want {
            mismatches.push(format!(
                "case {index}: {case}\n  rust: {got}\n  go:   {want}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} cases differ from the Go module:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches[..mismatches.len().min(10)].join("\n")
    );
}
