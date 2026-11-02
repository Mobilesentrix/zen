//! Statements that write: walk order, every run counted, a loop stops at a failure.

use crate::support::*;
use serde_json::{json, Value};
use zen_engine::{DecisionEngine, EvaluationOptions, Variable};

/// Evaluates `key` inside an evaluation scope (as the FFI does), so a read would be deduped.
fn eval_scoped(engine: &DecisionEngine, key: &str, input: Value) -> Result<Value, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(ms_zen_database::with_evaluation_scope(async {
        engine
            .evaluate_with_opts(
                key,
                Variable::from(input),
                EvaluationOptions {
                    trace: false,
                    max_depth: 5,
                },
            )
            .await
            .map(|r| serde_json::to_value(&r).unwrap())
            .map_err(|e| serde_json::to_value(&e).unwrap().to_string())
    }))
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn writing_statements_are_never_shared_and_a_loop_stops_at_its_first_failure() {
    let q = |id: &str, query: &str, extra: Value| {
        let mut config = json!({"query": query, "outputPath": id, "passThrough": false});
        if let (Some(c), Some(e)) = (config.as_object_mut(), extra.as_object()) {
            c.extend(e.clone());
        }
        db_node(id, config)
    };
    let engine = engine_with(
        databases(json!({})),
        vec![
            (
                "setup",
                graph(vec![
                    q("drop", "DROP TABLE IF EXISTS ms_write_log", json!({})),
                    q(
                        "create",
                        "CREATE TABLE ms_write_log (id INT PRIMARY KEY, tag VARCHAR(16))",
                        json!({}),
                    ),
                ]),
            ),
            (
                "twice",
                graph(vec![
                    q(
                        "a",
                        "INSERT INTO ms_write_log (id, tag) SELECT COALESCE(MAX(id), 0) + 1, {{tag}} FROM ms_write_log",
                        json!({"passThrough": true}),
                    ),
                    q(
                        "b",
                        "INSERT INTO ms_write_log (id, tag) SELECT COALESCE(MAX(id), 0) + 1, {{tag}} FROM ms_write_log",
                        json!({"passThrough": true}),
                    ),
                ]),
            ),
            (
                "loop",
                graph(vec![q(
                    "ins",
                    "INSERT INTO ms_write_log (id, tag) VALUES ({{id}}, 'loop')",
                    json!({"executionMode": "loop", "inputField": "ids"}),
                )]),
            ),
            (
                "count",
                graph(vec![q(
                    "n",
                    "SELECT id, tag FROM ms_write_log ORDER BY id",
                    json!({}),
                )]),
            ),
        ],
    );
    eval(&engine, "setup", json!({})).unwrap();
    // two identical INSERTs inside one evaluation scope both run
    eval_scoped(&engine, "twice", json!({"tag": "w"})).unwrap();
    let rows = eval_scoped(&engine, "count", json!({})).unwrap()["result"]["n"]["rows"].clone();
    assert_eq!(rows, json!([{"id": 1, "tag": "w"}, {"id": 2, "tag": "w"}]));

    // ids 10, 11, 11 (duplicate key), 12: the loop stops at the duplicate, 12 is never written
    let e = eval_scoped(
        &engine,
        "loop",
        json!({"ids": [{"id": 10}, {"id": 11}, {"id": 11}, {"id": 12}]}),
    )
    .unwrap_err();
    assert!(e.contains("Duplicate"), "{e}");
    let rows = eval_scoped(&engine, "count", json!({})).unwrap()["result"]["n"]["rows"].clone();
    let ids: Vec<i64> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2, 10, 11]);
    eval(&engine, "setup", json!({})).unwrap();
}
