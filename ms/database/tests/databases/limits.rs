//! Decimal and bool binds, `maxRows` and `queryTimeout`.

use crate::support::*;
use serde_json::json;
use std::time::Instant;

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
