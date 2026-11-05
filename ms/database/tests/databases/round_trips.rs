//! Per-evaluate de-duplication, `sessionVariables` and `testBeforeAcquire`.

use crate::support::*;
use ms_zen_database::Databases;
use serde_json::json;
use zen_engine::{EvaluationOptions, Variable};

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
