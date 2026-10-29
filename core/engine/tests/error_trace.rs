//! MobileSentrix: a traced evaluation error carries, on the failing node's trace entry, what
//! failed (`error`: kind, message, part, expression, key, missing, nodeId). Successful entries and
//! untraced errors keep upstream's shape (MS-FFI.md "Error trace").

use serde_json::{json, Value};
use std::sync::Arc;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{DecisionEngine, EvaluationOptions, EvaluationTraceKind};

fn node(id: &str, kind: &str, content: Value) -> Value {
    json!({"id": id, "name": id, "type": kind, "position": {"x": 0, "y": 0}, "content": content})
}

fn expression(id: &str, key: &str, value: &str) -> Value {
    node(
        id,
        "expressionNode",
        json!({"expressions": [{"id": format!("{id}-1"), "key": key, "value": value}],
               "passThrough": true}),
    )
}

/// Request -> nodes... -> Response, in a line.
fn graph(nodes: Vec<Value>) -> DecisionContent {
    let mut all = vec![json!({"id": "in", "name": "Request", "type": "inputNode",
                               "position": {"x": 0, "y": 0}})];
    all.extend(nodes);
    all.push(
        json!({"id": "out", "name": "Response", "type": "outputNode",
                     "position": {"x": 0, "y": 0}}),
    );
    let edges: Vec<Value> = all
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            json!({"id": format!("e{i}"), "type": "edge", "sourceId": w[0]["id"],
                   "targetId": w[1]["id"]})
        })
        .collect();
    serde_json::from_value(json!({"nodes": all, "edges": edges})).unwrap()
}

fn engine(decisions: Vec<(&str, DecisionContent)>) -> DecisionEngine {
    let loader = MemoryLoader::default();
    for (key, content) in decisions {
        loader.add(key, content);
    }
    DecisionEngine::default().with_loader(Arc::new(loader))
}

/// The error JSON the FFI emits (trace serialised in Default mode when traced).
async fn failure(engine: &DecisionEngine, key: &str, input: Value, trace: bool) -> Value {
    let error = engine
        .evaluate_with_opts(
            key,
            input.into(),
            EvaluationOptions {
                trace,
                max_depth: 5,
            },
        )
        .await
        .expect_err("the evaluation fails");
    let mode = match trace {
        true => EvaluationTraceKind::Default,
        false => EvaluationTraceKind::None,
    };
    error
        .serialize_with_mode(serde_json::value::Serializer, mode)
        .unwrap()
}

#[tokio::test]
async fn expression_node_failure_names_the_expression_its_kind_and_the_missing_values() {
    let engine = engine(vec![(
        "rule",
        graph(vec![
            expression("ok", "k", "1"),
            expression("fail", "margin", "missing.price - cost"),
        ]),
    )]);
    let error = failure(&engine, "rule", json!({"sku": "107000001022"}), true).await;

    assert_eq!(
        error["source"],
        json!(r#"Failed to evaluate expression: "missing.price - cost""#)
    );
    assert_eq!(error["nodeId"], json!("fail"));
    let trace = &error["trace"];
    assert_eq!(
        trace["fail"]["error"],
        json!({
            "kind": "missingValue",
            "message": "VM error: Opcode Subtract: Unsupported type",
            "part": "expression",
            "expression": "missing.price - cost",
            "key": "margin",
            "missing": ["missing.price", "cost"]
        })
    );
    assert_eq!(
        trace["fail"]["input"]["k"],
        json!(1),
        "the failing node's input is kept"
    );
    // completed steps keep upstream's shape: no error key at all
    for id in ["in", "ok"] {
        assert!(trace[id].get("error").is_none(), "{id}: {trace}");
    }
    assert_eq!(trace["ok"]["traceData"], json!({"k": {"result": 1}}));
}

#[tokio::test]
async fn expression_node_parse_and_type_errors() {
    let engine = engine(vec![
        ("parse", graph(vec![expression("fail", "x", "price +* 2")])),
        ("type", graph(vec![expression("fail", "x", "name - 1")])),
    ]);
    let parse = failure(&engine, "parse", json!({"price": 1}), true).await;
    assert_eq!(parse["trace"]["fail"]["error"]["kind"], json!("parse"));
    assert_eq!(
        parse["trace"]["fail"]["error"]["expression"],
        json!("price +* 2")
    );

    let wrong = failure(&engine, "type", json!({"name": "x"}), true).await;
    assert_eq!(wrong["trace"]["fail"]["error"]["kind"], json!("type"));
    assert!(wrong["trace"]["fail"]["error"].get("missing").is_none());
}

#[tokio::test]
async fn decision_table_failure_names_its_input_field() {
    let table = |extra: Value| {
        let mut content = json!({
            "hitPolicy": "first",
            "inputs": [{"id": "i1", "field": "qty"}],
            "outputs": [{"id": "o1", "field": "band"}],
            "rules": [{"_id": "r1", "i1": "> 10", "o1": "'big'"}, {"_id": "r2", "i1": "", "o1": "'small'"}]
        });
        content
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        node("table", "decisionTableNode", content)
    };
    let engine = engine(vec![
        (
            "field",
            graph(vec![table(
                json!({"inputField": "order.lines[0].qty * factor"}),
            )]),
        ),
        (
            "loop",
            graph(vec![table(
                json!({"inputField": "order", "executionMode": "loop"}),
            )]),
        ),
    ]);

    let error = failure(&engine, "field", json!({"order": {}}), true).await;
    assert_eq!(error["nodeId"], json!("table"));
    assert_eq!(
        error["source"],
        json!("Failed to evaluate expression: VM error: Opcode Multiply: Unsupported type")
    );
    assert_eq!(
        error["trace"]["table"]["error"],
        json!({
            "kind": "missingValue",
            "message": "VM error: Opcode Multiply: Unsupported type",
            "part": "inputField",
            "expression": "order.lines[0].qty * factor",
            "missing": ["order.lines", "factor"]
        })
    );

    let looped = failure(&engine, "loop", json!({"order": {"id": 1}}), true).await;
    assert_eq!(looped["source"], json!("Expected an array"));
    assert_eq!(
        looped["trace"]["table"]["error"],
        json!({"kind": "type", "message": "Expected an array, got object",
               "part": "inputField", "expression": "order"})
    );
}

#[tokio::test]
async fn sub_decision_failure_points_at_the_step_inside() {
    let engine = engine(vec![
        ("inner", graph(vec![expression("calc", "x", "a.b * 2")])),
        (
            "outer",
            graph(vec![node("sub", "decisionNode", json!({"key": "inner"}))]),
        ),
    ]);
    let error = failure(&engine, "outer", json!({"a": {"b": null}}), true).await;
    let outer = &error["trace"]["sub"];
    assert_eq!(outer["error"]["part"], json!("subDecision"));
    assert_eq!(outer["error"]["nodeId"], json!("calc"));
    assert_eq!(outer["error"]["kind"], json!("missingValue"));
    assert_eq!(outer["error"]["expression"], json!("a.b * 2"));
    assert_eq!(outer["error"]["missing"], json!(["a.b"]));
    // the inner trace (traceData of the sub-decision) has the same entry on the inner step
    assert_eq!(outer["traceData"]["calc"]["error"]["key"], json!("x"));
    assert!(outer["traceData"]["in"].get("error").is_none());
}

#[tokio::test]
async fn untraced_errors_and_successful_traces_are_unchanged() {
    let engine = engine(vec![
        (
            "fail",
            graph(vec![expression("fail", "margin", "missing.price - cost")]),
        ),
        ("ok", graph(vec![expression("ok", "k", "1")])),
    ]);
    let untraced = failure(&engine, "fail", json!({}), false).await;
    assert_eq!(
        untraced,
        json!({"type": "NodeError", "nodeId": "fail",
               "source": r#"Failed to evaluate expression: "missing.price - cost""#})
    );

    let response = engine
        .evaluate_with_opts(
            "ok",
            json!({}).into(),
            EvaluationOptions {
                trace: true,
                max_depth: 5,
            },
        )
        .await
        .unwrap();
    let response = serde_json::to_value(&response).unwrap();
    for (id, entry) in response["trace"].as_object().unwrap() {
        assert!(entry.get("error").is_none(), "{id}: {entry}");
    }
}
