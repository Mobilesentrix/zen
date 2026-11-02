//! Decoding, binding, node options and evaluation end to end.

use crate::support::*;
use ms_zen_database::Databases;
use serde_json::{json, Value};
use std::time::Instant;
use zen_engine::model::DecisionContent;
use zen_engine::{EvaluationOptions, Variable};

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn mysql_types_lists_and_json_strings() {
    let _ = NEEDS_DB;
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "d",
            graph(vec![db_node(
                "db",
                json!({
                    "query": "SELECT * FROM products WHERE site = {{site}} AND sku IN ({{skus}}) ORDER BY entity_id",
                    "outputPath": "db", "passThrough": false
                }),
            )]),
        )],
    );
    let r = eval(
        &engine,
        "d",
        json!({"site": "us", "skus": ["SKU-A", "SKU-B", "SKU-C"]}),
    )
    .unwrap();
    let db = &r["result"]["db"];
    assert_eq!(db["rowsCount"], json!(2));
    let a = &db["rows"][0];
    assert_eq!(a["entity_id"], json!(1));
    assert_eq!(a["sku"], json!("SKU-A"));
    assert_eq!(a["price"].as_f64(), Some(12.5));
    assert_eq!(a["weight"], json!(0.25));
    assert_eq!(a["qty"], json!(10));
    assert_eq!(a["big"].to_string(), "9007199254740993");
    assert_eq!(
        a["active"],
        json!(true),
        "sqlx reports TINYINT(1) as BOOLEAN (same as the 0.55 fork)"
    );
    assert_eq!(a["created_on"], json!("2024-01-02"));
    assert_eq!(a["updated_at"], json!("2024-01-02 03:04:05"));
    assert_eq!(a["meta"], json!({"color": "black", "tags": ["a", "b"]}));
    assert_eq!(
        a["meta_text"],
        json!({"nested": {"x": 1}}),
        "JSON-shaped text is parsed"
    );
    assert_eq!(a["blob_col"], json!("AP8="));
    let b = &db["rows"][1];
    assert_eq!(b["big"], Value::Null);
    assert_eq!(b["meta"], Value::Null);
    assert_eq!(b["meta_text"], json!([1, 2, 3]));
    // trace carries the raw db output
    let trace = &r["trace"]["db"]["traceData"];
    assert_eq!(trace["rowsCount"], json!(2));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn postgres_types_and_numbered_binds() {
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "d",
            graph(vec![db_node(
                "db",
                json!({
                    "instance": "pg-instance",
                    "query": "SELECT * FROM customers WHERE site = {{site}} AND id IN ({{ids}}) AND vip = {{vip}} ORDER BY id",
                    "outputPath": "db", "passThrough": false
                }),
            )]),
        )],
    );
    let r = eval(
        &engine,
        "d",
        json!({"site": "us", "ids": [1, 2, 3], "vip": true}),
    )
    .unwrap();
    let db = &r["result"]["db"];
    assert_eq!(db["rowsCount"], json!(1));
    let c = &db["rows"][0];
    assert_eq!(c["id"], json!(1));
    assert_eq!(c["email"], json!("a@example.com"));
    assert_eq!(c["tier"], json!(1));
    assert_eq!(c["credit"].as_f64(), Some(100.5));
    assert_eq!(c["score"], json!(0.75));
    assert_eq!(c["vip"], json!(true));
    assert_eq!(c["big"].to_string(), "9007199254740993");
    assert_eq!(c["joined_on"], json!("2024-01-02"));
    assert_eq!(c["updated_at"], json!("2024-01-02 03:04:05"));
    assert_eq!(c["seen_at"], json!("2024-01-02T03:04:05+00:00"));
    assert_eq!(c["profile"], json!({"plan": "gold"}));
    assert_eq!(c["tags"], json!(["x", "y"]));
    assert_eq!(c["ext_id"], json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"));
    assert_eq!(c["raw"], json!("AP8="));

    let r = eval(
        &engine,
        "d",
        json!({"site": "us", "ids": [2], "vip": false}),
    )
    .unwrap();
    let row = &r["result"]["db"]["rows"][0];
    assert_eq!(row["credit"].as_f64(), Some(0.0));
    assert_eq!(row["seen_at"], Value::Null);
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn end_to_end_request_database_expression_response() {
    let decision: DecisionContent = serde_json::from_value(json!({
        "nodes": [
            {"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}},
            {"id": "db", "name": "products", "type": "customNode", "position": {"x": 0, "y": 0},
             "content": {"kind": "database", "config": {
                 "instance": "starrocks",
                 "query": "SELECT sku, price, qty FROM products WHERE site = {{site ?? 'us'}} AND sku IN ({{skus}}) ORDER BY entity_id",
                 "outputPath": "dbResult", "passThrough": true }}},
            {"id": "expr", "name": "totals", "type": "expressionNode", "position": {"x": 0, "y": 0},
             "content": {"expressions": [
                 {"id": "x1", "key": "stockValue", "value": "sum(map(dbResult.rows, #.price * #.qty))"},
                 {"id": "x2", "key": "found", "value": "map(dbResult.rows, #.sku)"},
                 {"id": "x3", "key": "requested", "value": "len(skus)"}
             ], "passThrough": false}},
            {"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}
        ],
        "edges": [
            {"id": "e1", "type": "edge", "sourceId": "in", "targetId": "db"},
            {"id": "e2", "type": "edge", "sourceId": "db", "targetId": "expr"},
            {"id": "e3", "type": "edge", "sourceId": "expr", "targetId": "out"}
        ]
    })).unwrap();
    let engine = engine_with(databases(json!({})), vec![("stock.json", decision)]);
    let r = eval(
        &engine,
        "stock.json",
        json!({"skus": ["SKU-A", "SKU-B", "NOPE"]}),
    )
    .unwrap();
    assert_eq!(r["result"]["stockValue"], json!(185));
    assert_eq!(r["result"]["found"], json!(["SKU-A", "SKU-B"]));
    assert_eq!(r["result"]["requested"], json!(3));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn pass_through_output_path_defaults_and_nodes() {
    let engine = engine_with(
        databases(json!({})),
        vec![
            // editor shape: useCustomQuery/customQuery, no outputPath/passThrough -> dbResult + pass-through
            (
                "defaults",
                graph(vec![db_node(
                    "db",
                    json!({
                        "useCustomQuery": true,
                        "customQuery": "SELECT sku FROM products WHERE entity_id = {{id}}",
                        "selectedTables": [], "instance": "mysql-instance", "engine": "STARROCKS"
                    }),
                )]),
            ),
            (
                "nested",
                graph(vec![db_node(
                    "db",
                    json!({
                        "query": "SELECT qty FROM products WHERE entity_id = {{ $nodes.Request.id }}",
                        "outputPath": "a.b", "passThrough": true
                    }),
                )]),
            ),
        ],
    );
    let r = eval(&engine, "defaults", json!({"id": 3, "keep": "me"})).unwrap();
    assert_eq!(r["result"]["keep"], json!("me"));
    assert_eq!(r["result"]["dbResult"]["rows"], json!([{"sku": "SKU-C"}]));
    assert!(
        r["result"].get("$nodes").is_none(),
        "$nodes must not leak into output"
    );

    let r = eval(&engine, "nested", json!({"id": 2})).unwrap();
    assert_eq!(r["result"]["a"]["b"]["rows"], json!([{"qty": 3}]));
    assert_eq!(r["result"]["id"], json!(2));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn input_field_and_loop_mode() {
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "loop",
            graph(vec![db_node(
                "db",
                json!({
                    "query": "SELECT qty FROM products WHERE sku = {{sku}}",
                    "inputField": "lines", "executionMode": "loop",
                    "outputPath": "lines", "passThrough": true
                }),
            )]),
        )],
    );
    let r = eval(
        &engine,
        "loop",
        json!({"order": 9, "lines": [{"sku": "SKU-A", "n": 1}, {"sku": "SKU-B", "n": 2}]}),
    )
    .unwrap();
    let lines = &r["result"]["lines"];
    assert_eq!(lines[0]["n"], json!(1));
    assert_eq!(lines[0]["rows"], json!([{"qty": 10}]));
    assert_eq!(lines[1]["rows"], json!([{"qty": 3}]));
    assert_eq!(lines[1]["rowsCount"], json!(1));
    assert_eq!(r["result"]["order"], json!(9));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn multiple_instances_in_one_decision() {
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "both",
            graph(vec![
                db_node(
                    "my",
                    json!({"instance": "mysql-instance", "query": "SELECT COUNT(*) AS n FROM products WHERE site = {{site}}", "outputPath": "my"}),
                ),
                db_node(
                    "pg",
                    json!({"instance": "pg-instance", "query": "SELECT COUNT(*) AS n FROM customers WHERE site = {{site}}", "outputPath": "pg"}),
                ),
            ]),
        )],
    );
    let r = eval(&engine, "both", json!({"site": "us"})).unwrap();
    assert_eq!(r["result"]["my"]["rows"], json!([{"n": 3}]));
    assert_eq!(r["result"]["pg"]["rows"], json!([{"n": 2}]));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn injection_is_inert_against_a_real_database() {
    let engine = engine_with(
        databases(json!({})),
        vec![
            (
                "my",
                graph(vec![db_node(
                    "db",
                    json!({"query": "SELECT entity_id FROM products WHERE sku IN ({{skus}})", "passThrough": false}),
                )]),
            ),
            (
                "pg",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg-instance", "query": "SELECT id FROM customers WHERE email = {{email}}", "passThrough": false}),
                )]),
            ),
        ],
    );
    for evil in [
        "x' or '1'='1",
        "1) OR (1=1",
        "'; DROP TABLE products; --",
        "\\' OR 1=1 #",
    ] {
        let r = eval(&engine, "my", json!({"skus": [evil]})).unwrap();
        assert_eq!(r["result"]["dbResult"]["rowsCount"], json!(0), "{evil}");
        let r = eval(&engine, "pg", json!({"email": evil})).unwrap();
        assert_eq!(r["result"]["dbResult"]["rowsCount"], json!(0), "{evil}");
    }
    // a value containing a quote matches literally
    let r = eval(&engine, "my", json!({"skus": ["O'Brien"]})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"], json!([{"entity_id": 4}]));
    // table is still there
    let r = eval(&engine, "my", json!({"skus": ["SKU-A"]})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rowsCount"], json!(1));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn errors_are_node_errors_and_fallback_handles_other_kinds() {
    let engine = engine_with(
        databases(json!({})),
        vec![
            (
                "unknown",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "nope", "query": "SELECT 1"}),
                )]),
            ),
            (
                "bad-sql",
                graph(vec![db_node("db", json!({"query": "SELEC 1"}))]),
            ),
            (
                "null",
                graph(vec![db_node(
                    "db",
                    json!({"query": "SELECT {{customer_id}}"}),
                )]),
            ),
            (
                "template",
                graph(vec![db_node("db", json!({"query": "SELECT * FROM {{t}}"}))]),
            ),
            (
                "other",
                graph(vec![
                    json!({"id": "c", "name": "c", "type": "customNode", "position": {"x":0,"y":0},
                                         "content": {"kind": "sum", "config": {}}}),
                ]),
            ),
        ],
    );
    let e = eval(&engine, "unknown", json!({})).unwrap_err();
    assert!(
        e.contains("database instance \\\"nope\\\" is not configured"),
        "{e}"
    );
    let e = eval(&engine, "bad-sql", json!({})).unwrap_err();
    assert!(e.contains("Database query execution failed"), "{e}");
    let e = eval(&engine, "null", json!({"customer": 1})).unwrap_err();
    assert!(e.contains("resolved to null"), "{e}");
    let e = eval(&engine, "template", json!({"t": "x"})).unwrap_err();
    assert!(e.contains("Invalid database query template"), "{e}");
    let r = eval(&engine, "other", json!({})).unwrap();
    assert_eq!(r["result"]["fallbackKind"], json!("sum"));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn eager_connect_fails_fast_and_lazy_does_not() {
    let bad = json!({"connections": {"x": {"url": "mysql://root:test@127.0.0.1:1/none", "connectEagerly": true, "acquireTimeout": 2000}}});
    let err = Databases::new(serde_json::from_value(bad).unwrap()).unwrap_err();
    assert!(
        err.to_string().contains("Database connection error"),
        "{err}"
    );
    let lazy = json!({"connections": {"x": {"url": "mysql://root:test@127.0.0.1:1/none"}}});
    assert!(Databases::new(serde_json::from_value(lazy).unwrap()).is_ok());
    let bad_scheme = json!({"connections": {"x": {"url": "oracle://h/db"}}});
    assert!(Databases::new(serde_json::from_value(bad_scheme).unwrap())
        .unwrap_err()
        .to_string()
        .contains("unsupported database scheme"));
    // eager against the real DB works
    let ok =
        json!({"connections": {"x": {"url": env("MS_TEST_MYSQL_URL"), "connectEagerly": true}}});
    assert!(Databases::new(serde_json::from_value(ok).unwrap()).is_ok());
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn concurrent_evaluation_from_many_runtimes() {
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "d",
            graph(vec![db_node(
                "db",
                json!({"query": "SELECT qty FROM products WHERE entity_id = {{id}}", "passThrough": false}),
            )]),
        )],
    );
    let threads: Vec<_> = (0..16)
        .map(|t| {
            let engine = engine.clone();
            std::thread::spawn(move || {
                for i in 0..25u32 {
                    let id = (t + i) % 3 + 1;
                    let r = eval(&engine, "d", json!({"id": id})).unwrap();
                    let expected = [10, 3, 100][(id - 1) as usize];
                    assert_eq!(r["result"]["dbResult"]["rows"][0]["qty"], json!(expected));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

/// Timed micro-benchmark: same decision shape with and without a DB node.
/// `cargo test ... bench_ -- --ignored --nocapture`
#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn bench_decision_with_and_without_db_node() {
    let expr = json!({"id": "expr", "name": "e", "type": "expressionNode", "position": {"x":0,"y":0},
        "content": {"expressions": [{"id": "x", "key": "n", "value": "len(skus) * 2"}], "passThrough": true}});
    let db = db_node(
        "db",
        json!({"query": "SELECT sku, price, qty FROM products WHERE site = {{site}} AND sku IN ({{skus}})"}),
    );
    let pg = db_node(
        "db",
        json!({"instance": "pg-instance", "query": "SELECT id, email FROM customers WHERE site = {{site}} AND id IN ({{ids}})"}),
    );
    let dbs = databases(json!({}));
    let engine = engine_with(
        dbs.clone(),
        vec![
            ("plain", graph(vec![expr.clone()])),
            ("mysql", graph(vec![db, expr.clone()])),
            ("pg", graph(vec![pg, expr])),
        ],
    );
    let input = json!({"site": "us", "skus": ["SKU-A", "SKU-B"], "ids": [1, 2]});
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let run = |key: &str, n: u32| {
        rt.block_on(async {
            let start = Instant::now();
            for _ in 0..n {
                engine
                    .evaluate_with_opts(
                        key,
                        Variable::from(input.clone()),
                        EvaluationOptions {
                            trace: false,
                            max_depth: 5,
                        },
                    )
                    .await
                    .unwrap();
            }
            start.elapsed() / n
        })
    };
    // Baseline: the same SQL straight through the pool (no engine, no placeholder work).
    let conn = dbs.resolve(Some("mysql-instance")).unwrap();
    let raw = |n: u32| {
        rt.block_on(async {
            let start = Instant::now();
            for _ in 0..n {
                let q = ms_zen_database::BoundQuery {
                    sql: "SELECT sku, price, qty FROM products WHERE site = ? AND sku IN (?, ?)"
                        .into(),
                    params: vec![
                        ms_zen_database::BindValue::Text("us".into()),
                        ms_zen_database::BindValue::Text("SKU-A".into()),
                        ms_zen_database::BindValue::Text("SKU-B".into()),
                    ],
                };
                dbs.execute(&conn, q).await.unwrap();
            }
            start.elapsed() / n
        })
    };
    raw(50);
    println!(
        "BENCH   raw: {:?} per query (mysql pool only, 2000 runs)",
        raw(2000)
    );
    for key in ["plain", "mysql", "pg"] {
        run(key, 50); // warm-up: pools connect, statements get prepared
        let avg = run(key, 2000);
        println!("BENCH {key:>5}: {avg:?} per evaluation (2000 runs, sequential, warm pool)");
    }
}
