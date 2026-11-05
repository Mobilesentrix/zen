//! resultHandling against the corpus test databases (engine-v2/testdb: StarRocks 3.5 + Postgres
//! 16 on docker network `ms-ruleengine-testdb`). Needs MS_TESTDB_STARROCKS_URL and
//! MS_TESTDB_PG_URL; see MS-BUILD.md ("Integration tests").

use ms_zen_database::{DatabaseConfig, DatabaseCustomNode, Databases};
use serde_json::{json, Value};
use std::sync::Arc;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{DecisionEngine, EvaluationOptions, Variable};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set; start engine-v2/testdb"))
}

fn engine(decisions: Vec<(&str, Value)>) -> DecisionEngine {
    let config: DatabaseConfig = serde_json::from_value(json!({
        "connections": {
            "starrocks": { "url": env("MS_TESTDB_STARROCKS_URL"), "maxConnections": 4 },
            "pg": { "url": env("MS_TESTDB_PG_URL"), "maxConnections": 4 }
        },
        "default": "starrocks"
    }))
    .unwrap();
    let loader = MemoryLoader::default();
    for (key, config) in decisions {
        let content: DecisionContent = serde_json::from_value(json!({
            "nodes": [
                {"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}},
                {"id": "db", "name": "Look up product", "type": "customNode", "position": {"x": 0, "y": 0},
                 "content": {"kind": "database", "config": config}},
                {"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}
            ],
            "edges": [
                {"id": "e1", "type": "edge", "sourceId": "in", "targetId": "db"},
                {"id": "e2", "type": "edge", "sourceId": "db", "targetId": "out"}
            ]
        }))
        .unwrap();
        loader.add(key, content);
    }
    let dbs = Arc::new(Databases::new(config).unwrap());
    DecisionEngine::new(Arc::new(loader), Arc::new(DatabaseCustomNode::new(dbs)))
}

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
                    trace: false,
                    max_depth: 5,
                },
            )
            .await
            .map(|r| serde_json::to_value(&r).unwrap())
            .map_err(|e| serde_json::to_value(&e).unwrap().to_string())
    })
}

/// ms_analytics.catalog_product: sku 107011215481 has a DELETED duplicate listing (ids 3 and 26).
const STARROCKS_SQL: &str =
    "SELECT id, code, sku, status FROM ms_analytics.catalog_product WHERE sku = {{sku}} ORDER BY id";
/// samplePg.orders: customer 5001 has three orders, increment ids are unique.
const PG_SQL: &str =
    "SELECT increment_id, status FROM orders WHERE customer_id = {{customer}} ORDER BY id";

#[test]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
fn starrocks_none_one_several() {
    let engine = engine(vec![
        (
            "today",
            json!({"query": STARROCKS_SQL, "passThrough": false}),
        ),
        (
            "single",
            json!({"query": STARROCKS_SQL, "passThrough": false,
                          "resultHandling": {"none": "error", "noneMessage": "Not found", "one": "use", "several": "error", "severalMessage": "Duplicates found"}}),
        ),
        (
            "first",
            json!({"query": STARROCKS_SQL, "passThrough": false,
                         "resultHandling": {"none": "empty", "one": "use", "several": "first"}}),
        ),
    ]);
    // the plain shape is untouched
    let r = eval(&engine, "today", json!({"sku": "107000001022"})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"rows": [{"id": 1, "code": 1001, "sku": "107000001022", "status": "ENABLED"}], "rowsCount": 1})
    );
    // one -> object
    let r = eval(&engine, "single", json!({"sku": "107000001022"})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"id": 1, "code": 1001, "sku": "107000001022", "status": "ENABLED", "rowsCount": 1})
    );
    // several -> error with the analyst's message
    let e = eval(&engine, "single", json!({"sku": "107011215481"})).unwrap_err();
    assert!(e.contains("\"source\":\"Duplicates found\""), "{e}");
    // none -> error
    let e = eval(&engine, "single", json!({"sku": "000000000000"})).unwrap_err();
    assert!(e.contains("\"source\":\"Not found\""), "{e}");
    // first / empty
    let r = eval(&engine, "first", json!({"sku": "107011215481"})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"id": 3, "code": 1003, "sku": "107011215481", "status": "ENABLED", "rowsCount": 2})
    );
    let r = eval(&engine, "first", json!({"sku": "000000000000"})).unwrap();
    assert_eq!(r["result"]["dbResult"], json!({"rowsCount": 0}));
}

#[test]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
fn postgres_none_one_several() {
    let engine = engine(vec![
        (
            "today",
            json!({"instance": "pg", "query": PG_SQL, "passThrough": false}),
        ),
        (
            "single",
            json!({"instance": "pg", "query": PG_SQL, "passThrough": false,
                          "resultHandling": {"none": "error", "one": "use", "several": "error"}}),
        ),
        (
            "first",
            json!({"instance": "pg", "query": PG_SQL, "passThrough": false,
                         "resultHandling": {"none": "empty", "one": "use", "several": "first"}}),
        ),
    ]);
    let r = eval(&engine, "today", json!({"customer": 5002})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"rows": [{"increment_id": "UK100000101", "status": "canceled"}], "rowsCount": 1})
    );
    let r = eval(&engine, "single", json!({"customer": 5002})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"increment_id": "UK100000101", "status": "canceled", "rowsCount": 1})
    );
    let e = eval(&engine, "single", json!({"customer": 5001})).unwrap_err();
    assert!(e.contains("\"source\":\"Duplicates found\""), "{e}");
    let e = eval(&engine, "single", json!({"customer": 1})).unwrap_err();
    assert!(e.contains("\"source\":\"Not found\""), "{e}");
    let r = eval(&engine, "first", json!({"customer": 5001})).unwrap();
    assert_eq!(
        r["result"]["dbResult"],
        json!({"increment_id": "US100000101", "status": "processing", "rowsCount": 3})
    );
    let r = eval(&engine, "first", json!({"customer": 1})).unwrap();
    assert_eq!(r["result"]["dbResult"], json!({"rowsCount": 0}));
}

/// product_catalog.products: cost / price are DECIMAL(12,4); 8 rows, costs 9.62 .. 38.49.
const SR_BY_COST: &str =
    "SELECT t0.sku, t0.cost, t0.price FROM product_catalog.products t0 WHERE t0.cost = {{ cost }}";
const SR_COUNT_ABOVE: &str =
    "SELECT COUNT(*) AS n FROM product_catalog.products WHERE cost > {{ min }}";
const SR_IN_COSTS: &str =
    "SELECT sku FROM product_catalog.products WHERE cost IN ({{ costs }}) ORDER BY sku";

/// `WHERE cost = {{ cost }}` on a DECIMAL column: StarRocks rejects a NEWDECIMAL parameter
/// ("unknown mysql type code 246"). Decimal parameters must bind in a
/// form StarRocks accepts AND compare exactly; DECIMAL results must come back as numbers.
#[test]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
fn starrocks_decimal_parameters_and_results() {
    let engine = engine(vec![
        ("byCost", json!({"query": SR_BY_COST, "passThrough": false})),
        (
            "countAbove",
            json!({"query": SR_COUNT_ABOVE, "passThrough": false}),
        ),
        (
            "inCosts",
            json!({"query": SR_IN_COSTS, "passThrough": false}),
        ),
    ]);

    // decimal equality parameter
    let r = eval(&engine, "byCost", json!({"cost": 9.62})).unwrap();
    let rows = r["result"]["dbResult"]["rows"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["sku"], json!("107000001022"));
    // DECIMAL(12,4) result columns decode as exact numbers, not strings
    assert!(
        rows[0]["cost"].is_number(),
        "cost decoded as {:?}",
        rows[0]["cost"]
    );
    assert_eq!(rows[0]["cost"].as_f64(), Some(9.62));
    assert_eq!(rows[0]["price"].as_f64(), Some(17.49));

    // exact comparison: a value off by one unit in the 4th place matches nothing
    let r = eval(&engine, "byCost", json!({"cost": 9.6201})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rowsCount"], json!(0));
    let r = eval(&engine, "byCost", json!({"cost": 26.12})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"][0]["sku"],
        json!("107082074715")
    );

    // integer parameter and float parameter against the DECIMAL column
    let r = eval(&engine, "countAbove", json!({"min": 20})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"][0]["n"], json!(5));
    let r = eval(&engine, "countAbove", json!({"min": 21.99})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"][0]["n"], json!(4));

    // a list of decimals
    let r = eval(&engine, "inCosts", json!({"costs": [9.62, 13.74]})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"],
        json!([{"sku": "107000001022"}, {"sku": "107082994295"}])
    );
}

/// samplePg.orders.grand_total is NUMERIC(12,2): the Postgres path keeps its exact binding.
#[test]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
fn postgres_numeric_parameter_and_result() {
    let engine = engine(vec![(
        "byTotal",
        json!({"instance": "pg", "passThrough": false,
               "query": "SELECT increment_id, grand_total FROM orders WHERE grand_total = {{ total }} ORDER BY id"}),
    )]);
    let r = eval(&engine, "byTotal", json!({"total": 249.90})).unwrap();
    let rows = r["result"]["dbResult"]["rows"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["increment_id"], json!("US100000101"));
    assert!(rows[0]["grand_total"].is_number());
    assert_eq!(rows[0]["grand_total"].as_f64(), Some(249.9));
    let r = eval(&engine, "byTotal", json!({"total": 249.91})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rowsCount"], json!(0));
}

/// Fixed values from the studio's picture ({{ 9.62 }}, {{ '107000001022' }}) bind as parameters on StarRocks,
/// including a DECIMAL constant compared exactly with a DECIMAL(12,4) column.
#[test]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
fn starrocks_fixed_value_constants_bind() {
    let engine = engine(vec![
        (
            "decimal",
            json!({"passThrough": false,
                   "query": "SELECT t0.sku FROM product_catalog.products t0 WHERE t0.cost = {{ 9.62 }}"}),
        ),
        (
            "text",
            json!({"passThrough": false,
                   "query": "SELECT t0.cost FROM product_catalog.products t0 WHERE t0.sku = {{ '107000001022' }}"}),
        ),
        (
            "intAndBool",
            json!({"passThrough": false,
                   "query": "SELECT COUNT(*) AS n FROM product_catalog.products WHERE cost > {{ 20 }} AND {{ true }}"}),
        ),
        (
            "requestBool",
            json!({"passThrough": false,
                   "query": "SELECT COUNT(*) AS n FROM product_catalog.products WHERE cost > {{ 20 }} AND {{ live }}"}),
        ),
    ]);
    let r = eval(&engine, "decimal", json!({})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"],
        json!([{"sku": "107000001022"}])
    );
    let r = eval(&engine, "text", json!({})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"][0]["cost"].as_f64(),
        Some(9.62)
    );
    let r = eval(&engine, "intAndBool", json!({})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"][0]["n"], json!(5));
    // a boolean from the request binds the same way (StarRocks rejects TINYINT UNSIGNED)
    let r = eval(&engine, "requestBool", json!({"live": true})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"][0]["n"], json!(5));
    let r = eval(&engine, "requestBool", json!({"live": false})).unwrap();
    assert_eq!(r["result"]["dbResult"]["rows"][0]["n"], json!(0));
}

/// The same on Postgres: a NUMERIC(12,2) constant and a text constant bind exactly.
#[test]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
fn postgres_fixed_value_constants_bind() {
    let engine = engine(vec![
        (
            "decimal",
            json!({"instance": "pg", "passThrough": false,
                   "query": "SELECT increment_id FROM orders WHERE grand_total = {{ 249.90 }} ORDER BY id"}),
        ),
        (
            "text",
            json!({"instance": "pg", "passThrough": false,
                   "query": "SELECT grand_total FROM orders WHERE increment_id = {{ 'US100000101' }}"}),
        ),
    ]);
    let r = eval(&engine, "decimal", json!({})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"],
        json!([{"increment_id": "US100000101"}])
    );
    let r = eval(&engine, "text", json!({})).unwrap();
    assert_eq!(
        r["result"]["dbResult"]["rows"][0]["grand_total"].as_f64(),
        Some(249.9)
    );
}
