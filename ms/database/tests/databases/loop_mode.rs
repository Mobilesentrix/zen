//! Loop mode: concurrent, in order, identical items sent once.

use crate::support::*;
use serde_json::{json, Value};

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
