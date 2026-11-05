//! `resultHandling` against real databases.

use crate::support::*;
use serde_json::{json, Value};
use zen_engine::DecisionEngine;

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
