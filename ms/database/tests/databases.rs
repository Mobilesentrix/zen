//! Integration tests against real MySQL + PostgreSQL (ms-test/docker-compose.yml).
//! Run: see MS-BUILD.md ("Integration tests"). They need MS_TEST_MYSQL_URL / MS_TEST_PG_URL.

use ms_zen_database::{DatabaseConfig, DatabaseCustomNode, Databases};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest};
use zen_engine::nodes::{NodeResponse, NodeResult};
use zen_engine::{DecisionEngine, EvaluationOptions, Variable};

const NEEDS_DB: &str = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)";

fn env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set; start ms-test/docker-compose.yml"))
}

fn databases(extra: Value) -> Arc<Databases> {
    let mut cfg = json!({
        "connections": {
            "mysql-instance": { "url": env("MS_TEST_MYSQL_URL"), "aliases": ["starrocks"], "maxConnections": 8 },
            "pg-instance": { "url": env("MS_TEST_PG_URL"), "maxConnections": 8 }
        },
        "default": "mysql-instance"
    });
    if let (Some(c), Some(e)) = (cfg.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            c.insert(k.clone(), v.clone());
        }
    }
    let config: DatabaseConfig = serde_json::from_value(cfg).unwrap();
    Arc::new(Databases::new(config).unwrap())
}

/// Fallback adapter standing in for the Go custom-node callback.
#[derive(Debug)]
struct EchoFallback;

impl CustomNodeAdapter for EchoFallback {
    fn handle(&self, request: CustomNodeRequest) -> Pin<Box<dyn Future<Output = NodeResult> + '_>> {
        Box::pin(async move {
            Ok(NodeResponse {
                output: Variable::from(json!({ "fallbackKind": request.node.kind.as_ref() })),
                trace_data: None,
            })
        })
    }
}

fn db_node(id: &str, config: Value) -> Value {
    json!({ "id": id, "name": id, "type": "customNode", "position": {"x": 0, "y": 0},
            "content": { "kind": "database", "config": config } })
}

fn graph(nodes: Vec<Value>) -> DecisionContent {
    let mut all = vec![
        json!({"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}}),
    ];
    all.extend(nodes);
    all.push(json!({"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}));
    let edges: Vec<Value> = all
        .windows(2)
        .enumerate()
        .map(|(i, w)| json!({"id": format!("e{i}"), "type": "edge", "sourceId": w[0]["id"], "targetId": w[1]["id"]}))
        .collect();
    serde_json::from_value(json!({ "nodes": all, "edges": edges })).unwrap()
}

fn engine_with(dbs: Arc<Databases>, decisions: Vec<(&str, DecisionContent)>) -> DecisionEngine {
    let loader = MemoryLoader::default();
    for (k, d) in decisions {
        loader.add(k, d);
    }
    let adapter = DatabaseCustomNode::with_fallback(dbs, Box::new(EchoFallback));
    DecisionEngine::new(Arc::new(loader), Arc::new(adapter))
}

/// A current-thread runtime driven by block_on: the pools must work from an executor that
/// only runs inside the caller's block_on.
fn eval(engine: &DecisionEngine, key: &str, input: Value) -> Result<Value, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        engine
            .evaluate_with_opts(
                key,
                Variable::from(input),
                EvaluationOptions {
                    trace: true,
                    max_depth: 5,
                },
            )
            .await
            .map(|r| serde_json::to_value(&r).unwrap())
            .map_err(|e| serde_json::to_value(&e).unwrap().to_string())
    })
}

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

fn databases_from(cfg: Value) -> Arc<Databases> {
    Arc::new(Databases::new(serde_json::from_value(cfg).unwrap()).unwrap())
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn decimal_and_bool_binds_against_real_databases() {
    let engine = engine_with(
        databases(json!({})),
        vec![
            (
                "my",
                graph(vec![db_node(
                    "db",
                    json!({
                "query": "SELECT sku FROM products WHERE price >= {{minPrice}} AND active = {{flag}} ORDER BY entity_id",
                "passThrough": false }),
                )]),
            ),
            (
                "pg",
                graph(vec![db_node(
                    "db",
                    json!({
                "instance": "pg-instance",
                "query": "SELECT id FROM customers WHERE credit > {{minCredit}} AND vip = {{flag}} ORDER BY id",
                "passThrough": false }),
                )]),
            ),
        ],
    );
    let r = eval(&engine, "my", json!({"minPrice": 12.5, "flag": true})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"], json!([{"sku": "SKU-A"}]));
    let r = eval(&engine, "my", json!({"minPrice": 12.5, "flag": false})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"], json!([{"sku": "SKU-B"}]));
    let r = eval(&engine, "pg", json!({"minCredit": 5.2, "flag": true})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"],
        json!([{"id": 1}, {"id": 3}])
    );
    let r = eval(&engine, "pg", json!({"minCredit": 100.25, "flag": true})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"], json!([{"id": 1}]));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn max_rows_caps_result_sets() {
    let dbs = databases_from(json!({"connections": {
        "capped": { "url": env("MS_TEST_MYSQL_URL"), "maxRows": 3 },
        "exact":  { "url": env("MS_TEST_MYSQL_URL"), "maxRows": 4 },
        "pg":     { "url": env("MS_TEST_PG_URL"), "maxRows": 2 }
    }}));
    let engine = engine_with(
        dbs,
        vec![
            (
                "capped",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "capped", "query": "SELECT entity_id FROM products"}),
                )]),
            ),
            (
                "exact",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "exact", "query": "SELECT entity_id FROM products"}),
                )]),
            ),
            (
                "pg",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg", "query": "SELECT id FROM customers"}),
                )]),
            ),
            (
                "pg-limited",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg", "query": "SELECT id FROM customers LIMIT 2"}),
                )]),
            ),
        ],
    );
    let e = eval(&engine, "capped", json!({})).unwrap_err();
    assert!(e.contains("more than 3 rows"), "{e}");
    assert!(e.contains("maxRows"), "{e}");
    assert!(e.contains("capped"), "{e}");
    assert_eq!(
        eval(&engine, "exact", json!({})).unwrap()["result"]["dbResult"]["rowsCount"],
        json!(4)
    );
    let e = eval(&engine, "pg", json!({})).unwrap_err();
    assert!(e.contains("more than 2 rows"), "{e}");
    assert_eq!(
        eval(&engine, "pg-limited", json!({})).unwrap()["result"]["dbResult"]["rowsCount"],
        json!(2)
    );
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn query_timeout_aborts_slow_queries_and_the_pool_recovers() {
    let dbs = databases_from(json!({"connections": {
        "my": { "url": env("MS_TEST_MYSQL_URL"), "queryTimeout": 300, "maxConnections": 1 },
        "pg": { "url": env("MS_TEST_PG_URL"), "queryTimeout": 300, "maxConnections": 1 }
    }}));
    let engine = engine_with(
        dbs,
        vec![
            (
                "my-slow",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "my", "query": "SELECT SLEEP(2) AS s"}),
                )]),
            ),
            (
                "my-fast",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "my", "query": "SELECT {{n}} AS n"}),
                )]),
            ),
            (
                "pg-slow",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg", "query": "SELECT pg_sleep(2)"}),
                )]),
            ),
            (
                "pg-fast",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg", "query": "SELECT {{n}} AS n"}),
                )]),
            ),
        ],
    );
    for (slow, fast) in [("my-slow", "my-fast"), ("pg-slow", "pg-fast")] {
        let start = Instant::now();
        let e = eval(&engine, slow, json!({})).unwrap_err();
        assert!(
            start.elapsed().as_millis() < 1500,
            "{slow} took {:?}",
            start.elapsed()
        );
        assert!(e.contains("queryTimeout"), "{e}");
        assert!(e.contains("300 ms"), "{e}");
        // the single pooled connection is usable again (sqlx drains the abandoned result)
        let r = eval(&engine, fast, json!({"n": 7})).unwrap();
        assert_eq!(r["result"]["dbResult"]["rows"][0]["n"], json!(7));
    }
}

// ---------- resultHandling ----------

fn rh_engine(result_handling: Value) -> DecisionEngine {
    engine_with(
        databases(json!({})),
        vec![
            (
                "my",
                graph(vec![db_node(
                    "db",
                    json!({
                "query": "SELECT entity_id, sku, qty FROM products WHERE sku IN ({{skus}}) ORDER BY entity_id",
                "passThrough": false, "resultHandling": result_handling }),
                )]),
            ),
            (
                "pg",
                graph(vec![db_node(
                    "db",
                    json!({
                "instance": "pg-instance",
                "query": "SELECT id, email FROM customers WHERE site = {{site}} ORDER BY id",
                "passThrough": false, "resultHandling": result_handling }),
                )]),
            ),
        ],
    )
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn result_handling_default_is_byte_identical_to_today() {
    for rh in [
        Value::Null,
        json!({}),
        json!({"none": "continue", "one": "all", "several": "all"}),
    ] {
        let engine = rh_engine(rh.clone());
        let r = eval(&engine, "my", json!({"skus": ["SKU-A"]})).unwrap();
        assert_eq!(
            r["result"]["dbResult"],
            json!({"rows": [{"entity_id": 1, "sku": "SKU-A", "qty": 10}], "rowsCount": 1}),
            "{rh}"
        );
        let r = eval(&engine, "my", json!({"skus": ["NOPE"]})).unwrap();
        assert_eq!(
            r["result"]["dbResult"],
            json!({"rows": [], "rowsCount": 0}),
            "{rh}"
        );
        let r = eval(&engine, "pg", json!({"site": "us"})).unwrap();
        assert_eq!(r["result"]["dbResult"]["rowsCount"], json!(2), "{rh}");
        assert_eq!(r["trace"]["db"]["traceData"]["rowsCount"], json!(2));
    }
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn result_handling_single_row_as_object_and_first_of_several() {
    let engine = rh_engine(json!({"one": "use", "several": "first"}));
    let r = eval(&engine, "my", json!({"skus": ["SKU-B"]})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"entity_id": 2, "sku": "SKU-B", "qty": 3, "rowsCount": 1})
    );
    // trace keeps the raw result
    assert_eq!(
        r["trace"]["db"]["traceData"],
        json!({"rows": [{"entity_id": 2, "sku": "SKU-B", "qty": 3}], "rowsCount": 1})
    );
    let r = eval(&engine, "my", json!({"skus": ["SKU-A", "SKU-B", "SKU-C"]})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"entity_id": 1, "sku": "SKU-A", "qty": 10, "rowsCount": 3})
    );
    let r = eval(&engine, "pg", json!({"site": "us"})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"id": 1, "email": "a@example.com", "rowsCount": 2})
    );
    // none still continues with the plain shape
    let r = eval(&engine, "my", json!({"skus": ["NOPE"]})).unwrap();
    assert_eq!(r["result"]["dbResult"], json!({"rows": [], "rowsCount": 0}));
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn result_handling_errors_carry_the_analyst_message() {
    let engine = rh_engine(
        json!({"none": "error", "noneMessage": "No such product", "one": "use", "several": "error"}),
    );
    let e = eval(&engine, "my", json!({"skus": ["NOPE"]})).unwrap_err();
    assert!(e.contains("\"source\":\"No such product\""), "{e}");
    assert!(e.contains("\"nodeId\":\"db\""), "{e}");
    let e = eval(&engine, "pg", json!({"site": "us"})).unwrap_err();
    assert!(e.contains("\"source\":\"Duplicates found\""), "{e}");
    // default message for none
    let engine = rh_engine(json!({"none": "error"}));
    let e = eval(&engine, "my", json!({"skus": ["NOPE"]})).unwrap_err();
    assert!(e.contains("\"source\":\"Not found\""), "{e}");
    // empty: an object with only the count
    let engine = rh_engine(json!({"none": "empty", "one": "use"}));
    let r = eval(&engine, "my", json!({"skus": ["NOPE"]})).unwrap();
    assert_eq!(r["result"]["dbResult"], json!({"rowsCount": 0}));
    // unknown value is a config error naming the field
    let engine = rh_engine(json!({"none": "stop"}));
    let e = eval(&engine, "my", json!({"skus": ["SKU-A"]})).unwrap_err();
    assert!(
        e.contains("Invalid database node config") && e.contains("stop"),
        "{e}"
    );
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn result_handling_in_loop_mode_shapes_every_item() {
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "loop",
            graph(vec![db_node(
                "db",
                json!({
            "query": "SELECT qty FROM products WHERE sku = {{sku}}",
            "inputField": "lines", "executionMode": "loop", "outputPath": "stock", "passThrough": true,
            "resultHandling": {"none": "error", "noneMessage": "Unknown SKU", "one": "use", "several": "error"} }),
            )]),
        )],
    );
    let r = eval(
        &engine,
        "loop",
        json!({"lines": [{"sku": "SKU-A"}, {"sku": "SKU-B"}]}),
    )
    .unwrap();
    assert_eq!(
        r["result"]["stock"],
        json!([{"sku": "SKU-A", "qty": 10, "rowsCount": 1}, {"sku": "SKU-B", "qty": 3, "rowsCount": 1}])
    );
    let e = eval(
        &engine,
        "loop",
        json!({"lines": [{"sku": "SKU-A"}, {"sku": "NOPE"}]}),
    )
    .unwrap_err();
    assert!(e.contains("\"source\":\"Unknown SKU\""), "{e}");
}

// ---------- speed knobs: per-evaluate de-dup, sessionVariables, testBeforeAcquire ----------

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn identical_queries_share_one_round_trip_inside_one_evaluation_only() {
    // Two nodes with the same non-deterministic query: inside one evaluation scope they see the
    // same value (one round trip); across calls, or with dedupe off, they differ.
    let two_nodes = |instance: &str, sql: &str| {
        graph(vec![
            db_node(
                "a",
                json!({"instance": instance, "query": sql, "outputPath": "a"}),
            ),
            db_node(
                "b",
                json!({"instance": instance, "query": sql, "outputPath": "b"}),
            ),
        ])
    };
    let engine = engine_with(
        databases(json!({})),
        vec![
            (
                "my",
                two_nodes("mysql-instance", "SELECT UUID() AS u, {{n}} AS n"),
            ),
            (
                "pg",
                two_nodes(
                    "pg-instance",
                    "SELECT gen_random_uuid()::text AS u, {{n}} AS n",
                ),
            ),
        ],
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let scoped = |key: &str| {
        rt.block_on(ms_zen_database::with_evaluation_scope(async {
            let r = engine
                .evaluate_with_opts(
                    key,
                    Variable::from(json!({"n": 1})),
                    EvaluationOptions {
                        trace: false,
                        max_depth: 5,
                    },
                )
                .await
                .unwrap();
            serde_json::to_value(&r).unwrap()
        }))
    };
    for key in ["my", "pg"] {
        let r = scoped(key);
        let (a, b) = (
            &r["result"]["a"]["rows"][0]["u"],
            &r["result"]["b"]["rows"][0]["u"],
        );
        assert!(
            a.is_string() && a == b,
            "{key}: same evaluation must share the round trip: {a} vs {b}"
        );
        let again = scoped(key);
        assert_ne!(
            again["result"]["a"]["rows"][0]["u"], *a,
            "{key}: nothing survives to the next call"
        );
        // without a scope (plain Rust embedder) every node queries
        let plain = eval(&engine, key, json!({"n": 1})).unwrap();
        assert_ne!(
            plain["result"]["a"]["rows"][0]["u"], plain["result"]["b"]["rows"][0]["u"],
            "{key}"
        );
    }
    // dedupe: false on the connection -> two round trips even inside a scope
    let off = engine_with(
        databases_from(
            json!({"connections": {"my": {"url": env("MS_TEST_MYSQL_URL"), "dedupe": false}}}),
        ),
        vec![("my", two_nodes("my", "SELECT UUID() AS u, {{n}} AS n"))],
    );
    let r = rt.block_on(ms_zen_database::with_evaluation_scope(async {
        serde_json::to_value(
            off.evaluate_with_opts(
                "my",
                Variable::from(json!({"n": 1})),
                EvaluationOptions {
                    trace: false,
                    max_depth: 5,
                },
            )
            .await
            .unwrap(),
        )
        .unwrap()
    }));
    assert_ne!(
        r["result"]["a"]["rows"][0]["u"],
        r["result"]["b"]["rows"][0]["u"]
    );
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn session_variables_apply_to_every_connection() {
    let dbs = databases_from(json!({"connections": {
        "my": {"url": env("MS_TEST_MYSQL_URL"), "maxConnections": 2, "sessionVariables": {"sql_select_limit": "2"}},
        "pg": {"url": env("MS_TEST_PG_URL"), "maxConnections": 2, "sessionVariables": {"application_name": "ms-zen test", "statement_timeout": "2500"}}
    }}));
    let engine = engine_with(
        dbs,
        vec![
            (
                "my",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "my", "query": "SELECT entity_id FROM products ORDER BY entity_id", "passThrough": false}),
                )]),
            ),
            (
                "pg",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg", "query": "SELECT current_setting('application_name') AS app, current_setting('statement_timeout') AS t", "passThrough": false}),
                )]),
            ),
        ],
    );
    for _ in 0..3 {
        let r = eval(&engine, "my", json!({})).unwrap();
        assert_eq!(
            r["result"]["dbResult"]["rowsCount"],
            json!(2),
            "sql_select_limit applied on the pooled connection"
        );
        let r = eval(&engine, "pg", json!({})).unwrap();
        assert_eq!(
            r["result"]["dbResult"]["rows"][0],
            json!({"app": "ms-zen test", "t": "2500ms"})
        );
    }
    // an invalid name is a configuration error, before any connection is made
    let bad = json!({"connections": {"x": {"url": env("MS_TEST_MYSQL_URL"), "sessionVariables": {"a;b": "1"}}}});
    let err = Databases::new(serde_json::from_value(bad).unwrap())
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not an identifier"), "{err}");
    // a variable the server rejects fails the first query of that connection, loudly
    let rejected = engine_with(
        databases_from(
            json!({"connections": {"my": {"url": env("MS_TEST_MYSQL_URL"), "sessionVariables": {"no_such_variable_xyz": "1"}, "acquireTimeout": 1500}}}),
        ),
        vec![(
            "my",
            graph(vec![db_node(
                "db",
                json!({"instance": "my", "query": "SELECT 1 AS n"}),
            )]),
        )],
    );
    let err = eval(&rejected, "my", json!({})).unwrap_err();
    assert!(
        err.contains("no_such_variable_xyz") && err.contains("session variable statement"),
        "{err}"
    );
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn test_before_acquire_off_still_serves_queries() {
    let dbs = databases_from(json!({"connections": {
        "my": {"url": env("MS_TEST_MYSQL_URL"), "maxConnections": 1, "testBeforeAcquire": false},
        "pg": {"url": env("MS_TEST_PG_URL"), "maxConnections": 1, "testBeforeAcquire": false}
    }}));
    let engine = engine_with(
        dbs,
        vec![
            (
                "my",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "my", "query": "SELECT qty FROM products WHERE sku = {{sku}}", "passThrough": false}),
                )]),
            ),
            (
                "pg",
                graph(vec![db_node(
                    "db",
                    json!({"instance": "pg", "query": "SELECT tier FROM customers WHERE id = {{id}}", "passThrough": false}),
                )]),
            ),
        ],
    );
    for _ in 0..5 {
        assert_eq!(
            eval(&engine, "my", json!({"sku": "SKU-A"})).unwrap()["result"]["dbResult"]["rows"],
            json!([{"qty": 10}])
        );
        assert_eq!(
            eval(&engine, "pg", json!({"id": 1})).unwrap()["result"]["dbResult"]["rows"],
            json!([{"tier": 1}])
        );
    }
}

// ---------- loop mode: concurrent, in order, identical items sent once ----------

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn loop_mode_runs_items_concurrently_in_order_and_sends_duplicates_once() {
    let dbs = databases(json!({}));
    let engine = engine_with(
        dbs.clone(),
        vec![(
            "slow",
            graph(vec![db_node(
                "db",
                json!({
            "query": "SELECT {{ n }} AS n, SLEEP(0.2) AS slept",
            "inputField": "items", "executionMode": "loop", "outputPath": "out", "passThrough": false,
            "resultHandling": {"one": "use"} }),
            )]),
        )],
    );
    let items: Vec<Value> = (1..=8).map(|n| json!({"n": n})).collect();
    let before = dbs
        .stats()
        .into_iter()
        .find(|(n, _)| n == "mysql-instance")
        .unwrap()
        .1;
    let start = std::time::Instant::now();
    let r = eval(&engine, "slow", json!({"items": items})).unwrap();
    let took = start.elapsed();
    let ns: Vec<i64> = r["result"]["out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["n"].as_i64().unwrap())
        .collect();
    assert_eq!(
        ns,
        (1..=8).collect::<Vec<_>>(),
        "results stay in item order"
    );
    // eight 200 ms queries one after another would take 1.6 s
    assert!(
        took < std::time::Duration::from_millis(1000),
        "took {took:?}"
    );
    // identical items share one query
    let mid = dbs
        .stats()
        .into_iter()
        .find(|(n, _)| n == "mysql-instance")
        .unwrap()
        .1;
    let r = eval(
        &engine,
        "slow",
        json!({"items": [{"n": 1}, {"n": 2}, {"n": 1}, {"n": 1}]}),
    )
    .unwrap();
    let after = dbs
        .stats()
        .into_iter()
        .find(|(n, _)| n == "mysql-instance")
        .unwrap()
        .1;
    assert_eq!(mid.since(&before).queries, 8);
    assert_eq!(after.since(&mid).queries, 2);
    let ns: Vec<i64> = r["result"]["out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["n"].as_i64().unwrap())
        .collect();
    assert_eq!(ns, vec![1, 2, 1, 1]);
}

#[test]
#[ignore = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)"]
fn loop_mode_reports_the_first_failure_in_item_order() {
    let engine = engine_with(
        databases(json!({})),
        vec![(
            "div",
            graph(vec![db_node(
                "db",
                json!({
            "instance": "pg-instance",
            "query": "SELECT 10 / {{ d }}::int AS q",
            "inputField": "items", "executionMode": "loop", "outputPath": "out",
            "resultHandling": {"one": "use"} }),
            )]),
        )],
    );
    // item 1 fails in the database, item 2 would fail to bind: the database error is reported
    let e = eval(
        &engine,
        "div",
        json!({"items": [{"d": 1}, {"d": 0}, {"d": null}]}),
    )
    .unwrap_err();
    assert!(e.contains("division by zero"), "{e}");
    // item 1 fails to bind, item 2 would fail in the database: the bind error is reported
    let e = eval(
        &engine,
        "div",
        json!({"items": [{"d": 1}, {"d": null}, {"d": 0}]}),
    )
    .unwrap_err();
    assert!(e.contains("resolved to null"), "{e}");
    let r = eval(&engine, "div", json!({"items": [{"d": 2}, {"d": 5}]})).unwrap();
    assert_eq!(
        r["result"]["out"],
        json!([{"d": 2, "q": 5, "rowsCount": 1}, {"d": 5, "q": 2, "rowsCount": 1}])
    );
}

// ---------- statements that write: walk order, every run counted, loop stops at a failure ----------

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
