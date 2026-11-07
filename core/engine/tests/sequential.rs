//! `executionMode: "sequential"`: items evaluated one at a time, in order, with a state carried
//! from each item's output to the next item's input (docs/adr/0006-sequential-execution-mode.md).

use serde_json::{json, Value};
use std::sync::Arc;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{DecisionEngine, EvaluationOptions, EvaluationTraceKind};

fn node(id: &str, kind: &str, content: Value) -> Value {
    json!({"id": id, "name": id, "type": kind, "position": {"x": 0, "y": 0}, "content": content})
}

/// An expression node whose rows are `(key, value)`, with extra content (transform attributes).
fn expressions(id: &str, rows: &[(&str, &str)], extra: Value) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, (key, value))| json!({"id": format!("{id}-{i}"), "key": key, "value": value}))
        .collect();
    let mut content = json!({"expressions": rows});
    merge(&mut content, extra);
    node(id, "expressionNode", content)
}

fn merge(target: &mut Value, extra: Value) {
    target
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
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

async fn evaluate(engine: &DecisionEngine, key: &str, input: Value) -> Value {
    engine
        .evaluate(key, input.into())
        .await
        .unwrap_or_else(|error| panic!("{key} fails: {error:?}"))
        .result
        .to_value()
}

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

/// One step of "budget consumed in warehouse-id order", as a decision table: take what the
/// warehouse needs while the budget lasts.
fn consume_table() -> DecisionContent {
    graph(vec![node(
        "consume",
        "decisionTableNode",
        json!({
            "hitPolicy": "first",
            "inputs": [{"id": "i1", "field": "need - state.remaining"}],
            "outputs": [
                {"id": "o1", "field": "warehouseId"},
                {"id": "o2", "field": "take"},
                {"id": "o3", "field": "state.remaining"}
            ],
            "rules": [
                {"_id": "r1", "i1": "<= 0", "o1": "warehouseId", "o2": "need",
                 "o3": "state.remaining - need"},
                {"_id": "r2", "i1": "", "o1": "warehouseId", "o2": "state.remaining",
                 "o3": "0"}
            ]
        }),
    )])
}

fn budget_rule() -> DecisionContent {
    graph(vec![node(
        "allocate",
        "decisionNode",
        json!({
            "key": "consume",
            "inputField": "warehouses",
            "executionMode": "sequential",
            "stateField": "{remaining: budget}",
            "outputPath": "allocations",
        }),
    )])
}

fn budget_engine() -> DecisionEngine {
    engine(vec![
        ("consume", consume_table()),
        ("budget", budget_rule()),
    ])
}

fn warehouses() -> Value {
    json!({
        "budget": 100,
        "warehouses": [
            {"warehouseId": 1, "need": 30},
            {"warehouseId": 2, "need": 50.5},
            {"warehouseId": 3, "need": 40},
            {"warehouseId": 4, "need": 10}
        ]
    })
}

#[tokio::test]
async fn running_budget_is_consumed_in_input_order() {
    let result = evaluate(&budget_engine(), "budget", warehouses()).await;
    assert_eq!(
        result,
        json!({
            "allocations": [
                {"warehouseId": 1, "take": 30},
                {"warehouseId": 2, "take": 50.5},
                {"warehouseId": 3, "take": 19.5},
                {"warehouseId": 4, "take": 0}
            ],
            "allocationsState": {"remaining": 0}
        })
    );
}

#[tokio::test]
async fn sequential_runs_are_deterministic() {
    let engine = budget_engine();
    let first = evaluate(&engine, "budget", warehouses()).await;
    for _ in 0..50 {
        assert_eq!(evaluate(&engine, "budget", warehouses()).await, first);
    }
}

#[tokio::test]
async fn stop_ends_the_step_after_that_item() {
    let rule = graph(vec![expressions(
        "sum",
        &[
            ("state.sum", "state.sum + item"),
            ("total", "state.sum + item"),
            ("stop", "state.sum + item >= 10"),
        ],
        json!({
            "inputField": "numbers",
            "executionMode": "sequential",
            "stateField": "{sum: 0}",
            "outputPath": "steps",
        }),
    )]);
    let engine = engine(vec![("sum", rule)]);
    let result = evaluate(&engine, "sum", json!({"numbers": [1, 2, 3, 4, 5, 6]})).await;
    // state and stop are not part of the per-item results
    assert_eq!(
        result,
        json!({
            "steps": [{"total": 1}, {"total": 3}, {"total": 6}, {"total": 10}],
            "stepsState": {"sum": 10}
        })
    );
}

#[tokio::test]
async fn bisection_converges_with_decimal_state() {
    // 80 halvings of [0, 2] towards sqrt(2), stopping once the interval is narrow enough
    let mid = "(state.lo + state.hi) / 2";
    let below = format!("{mid} * {mid} < 2");
    let lo = format!("{below} ? {mid} : state.lo");
    let hi = format!("{below} ? state.hi : {mid}");
    let rule = graph(vec![expressions(
        "bisect",
        &[
            ("state.lo", lo.as_str()),
            ("state.hi", hi.as_str()),
            ("stop", "state.hi - state.lo < 0.0000000001"),
        ],
        json!({
            "inputField": "halvings",
            "executionMode": "sequential",
            "stateField": "{lo: 0, hi: 2}",
            "statePath": "root",
        }),
    )]);
    let engine = engine(vec![("bisect", rule)]);
    let result = evaluate(&engine, "bisect", json!({"halvings": vec![0; 80]})).await;
    let lo: f64 = result["root"]["lo"].as_f64().unwrap();
    assert!((lo - std::f64::consts::SQRT_2).abs() < 1e-9, "{result}");
    // only statePath set: no per-item results in the output
    assert_eq!(result.as_object().unwrap().len(), 1, "{result}");
}

#[tokio::test]
async fn empty_array_gives_no_results_and_the_initial_state() {
    let result = evaluate(
        &budget_engine(),
        "budget",
        json!({"budget": 7, "warehouses": []}),
    )
    .await;
    assert_eq!(
        result,
        json!({"allocations": [], "allocationsState": {"remaining": 7}})
    );
}

#[tokio::test]
async fn state_starts_null_without_a_state_field_and_is_kept_when_not_returned() {
    let rule = graph(vec![expressions(
        "count",
        &[
            ("state", "state == null ? 1 : state + 1"),
            ("seen", "state"),
        ],
        json!({"inputField": "items", "executionMode": "sequential", "outputPath": "out"}),
    )]);
    let keep = graph(vec![expressions(
        "keep",
        &[("label", "upper(name)")],
        json!({"inputField": "items", "executionMode": "sequential", "outputPath": "out",
               "stateField": "start", "passThrough": true}),
    )]);
    let engine = engine(vec![("count", rule), ("keep", keep)]);

    let result = evaluate(&engine, "count", json!({"items": ["a", "b", "c"]})).await;
    assert_eq!(
        result,
        json!({"out": [{"seen": null}, {"seen": 1}, {"seen": 2}], "outState": 3})
    );

    // no `state` in the output: the state is carried unchanged; passThrough merges the item
    // into its result and the node output into the node input
    let result = evaluate(
        &engine,
        "keep",
        json!({"start": {"n": 1}, "items": [{"name": "x"}, {"name": "y"}]}),
    )
    .await;
    assert_eq!(
        result,
        json!({
            "start": {"n": 1},
            "items": [{"name": "x"}, {"name": "y"}],
            "out": [{"name": "x", "label": "X"}, {"name": "y", "label": "Y"}],
            "outState": {"n": 1}
        })
    );
}

#[tokio::test]
async fn an_object_state_update_is_merged_and_other_values_replace_the_state() {
    let rule = graph(vec![expressions(
        "merge",
        &[("state", "patch")],
        json!({"inputField": "updates", "executionMode": "sequential", "stateField": "start",
               "statePath": "final"}),
    )]);
    let engine = engine(vec![("merge", rule)]);
    let result = evaluate(
        &engine,
        "merge",
        json!({
            "start": {"a": 1, "b": {"x": 1, "y": 2}, "list": [1, 2]},
            "updates": [{"patch": {"b": {"y": 3}}}, {"patch": {"a": null, "list": [9]}}]
        }),
    )
    .await;
    assert_eq!(
        result,
        json!({"final": {"b": {"x": 1, "y": 3}, "list": [9]}})
    );

    let result = evaluate(
        &engine,
        "merge",
        json!({"start": {"a": 1}, "updates": [{"patch": 5}, {"patch": {"c": 1}}]}),
    )
    .await;
    assert_eq!(result, json!({"final": {"c": 1}}));

    // a null final state is written as null; like every null field of a node output, the
    // graph's merge into the next node's input drops it
    let result = evaluate(&engine, "merge", json!({"updates": []})).await;
    assert_eq!(result, json!({}));
}

#[tokio::test]
async fn sequential_runs_inside_a_loop_with_its_own_state_per_group() {
    let inner = graph(vec![expressions(
        "run",
        &[("state", "state + qty"), ("running", "state + qty")],
        json!({"inputField": "rows", "executionMode": "sequential", "stateField": "0",
               "outputPath": "rows"}),
    )]);
    let outer = graph(vec![node(
        "groups",
        "decisionNode",
        json!({"key": "inner", "inputField": "groups", "executionMode": "loop",
               "outputPath": "groups"}),
    )]);
    let engine = engine(vec![("inner", inner), ("outer", outer)]);
    let result = evaluate(
        &engine,
        "outer",
        json!({"groups": [
            {"rows": [{"qty": 1}, {"qty": 2}]},
            {"rows": [{"qty": 10}, {"qty": 20}, {"qty": 30}]}
        ]}),
    )
    .await;
    assert_eq!(
        result,
        json!({"groups": [
            {"rows": [{"running": 1}, {"running": 3}], "rowsState": 3},
            {"rows": [{"running": 10}, {"running": 30}, {"running": 60}], "rowsState": 60}
        ]})
    );
}

#[tokio::test]
async fn a_failing_item_is_named_by_its_index() {
    let rule = graph(vec![expressions(
        "cost",
        &[("state", "state + price * qty")],
        json!({"inputField": "lines", "executionMode": "sequential", "stateField": "0",
               "outputPath": "lines"}),
    )]);
    let sub = graph(vec![node(
        "sub",
        "decisionNode",
        json!({"key": "cost-rule", "inputField": "orders", "executionMode": "sequential",
               "outputPath": "orders"}),
    )]);
    let engine = engine(vec![
        ("cost-rule", rule.clone()),
        ("cost", rule),
        ("nested", sub),
    ]);
    let input = json!({"lines": [{"price": 1, "qty": 1}, {"price": 2, "qty": 2},
                                 {"price": null, "qty": 3}]});

    let untraced = failure(&engine, "cost", input.clone(), false).await;
    assert_eq!(untraced["nodeId"], json!("cost"));
    assert_eq!(
        untraced["source"],
        json!(r#"item 2: Failed to evaluate expression: "state + price * qty""#)
    );

    let traced = failure(&engine, "cost", input.clone(), true).await;
    let error = &traced["trace"]["cost"]["error"];
    assert_eq!(error["itemIndex"], json!(2), "{traced}");
    assert_eq!(error["kind"], json!("missingValue"), "{traced}");
    assert_eq!(error["missing"], json!(["price"]), "{traced}");

    // a sequential sub-decision inside a sequential step: each level names its own item
    let nested = failure(
        &engine,
        "nested",
        json!({"orders": [{"lines": []}, input]}),
        true,
    )
    .await;
    let error = &nested["trace"]["sub"]["error"];
    assert_eq!(error["itemIndex"], json!(1), "{nested}");
    assert_eq!(error["part"], json!("subDecision"), "{nested}");
    assert!(
        nested["source"].as_str().unwrap().starts_with("item 1: "),
        "{nested}"
    );
}

#[tokio::test]
async fn max_iterations_guards_a_loop_that_never_stops() {
    let rule = |max: u32| {
        graph(vec![expressions(
            "walk",
            // the step sees only the item and the state: parameters travel in the state, and
            // an update of `state.n` keeps `state.limit`
            &[
                ("state.n", "state.n + 1"),
                ("stop", "state.n + 1 >= state.limit"),
            ],
            json!({"inputField": "items", "executionMode": "sequential",
                   "stateField": "{n: 0, limit: limit}",
                   "statePath": "steps", "maxIterations": max}),
        )])
    };
    let engine = engine(vec![("walk", rule(3))]);
    let error = failure(
        &engine,
        "walk",
        json!({"items": [0, 0, 0, 0, 0], "limit": 9}),
        true,
    )
    .await;
    assert_eq!(
        error["source"],
        json!("sequential execution reached maxIterations (3) before an item returned stop")
    );
    assert_eq!(error["trace"]["walk"]["error"]["part"], json!("config"));

    // stopping before the limit is fine, however long the input
    let result = evaluate(
        &engine,
        "walk",
        json!({"items": [0, 0, 0, 0, 0], "limit": 2}),
    )
    .await;
    assert_eq!(result, json!({"steps": {"n": 2, "limit": 2}}));
}

#[tokio::test]
async fn misconfigured_steps_fail_clearly() {
    let no_target = graph(vec![expressions(
        "x",
        &[("a", "1")],
        json!({"inputField": "items", "executionMode": "sequential"}),
    )]);
    let not_array = graph(vec![expressions(
        "x",
        &[("a", "1")],
        json!({"inputField": "items", "executionMode": "sequential", "outputPath": "o"}),
    )]);
    let bad_state = graph(vec![expressions(
        "x",
        &[("a", "1")],
        json!({"inputField": "items", "executionMode": "sequential", "outputPath": "o",
               "stateField": "budget.total * 2"}),
    )]);
    let engine = engine(vec![
        ("none", no_target),
        ("obj", not_array),
        ("state", bad_state),
    ]);

    let error = failure(&engine, "none", json!({"items": [1]}), false).await;
    assert_eq!(
        error["source"],
        json!("sequential execution needs an outputPath or a statePath")
    );

    let error = failure(&engine, "obj", json!({"items": {"a": 1}}), true).await;
    assert_eq!(error["source"], json!("Expected an array"));

    let error = failure(&engine, "state", json!({"items": [1]}), true).await;
    assert_eq!(error["trace"]["x"]["error"]["part"], json!("stateField"));
    assert_eq!(
        error["trace"]["x"]["error"]["missing"],
        json!(["budget.total"])
    );
}

#[tokio::test]
async fn traced_steps_keep_one_trace_entry_per_evaluated_item() {
    let response = budget_engine()
        .evaluate_with_opts(
            "budget",
            warehouses().into(),
            EvaluationOptions {
                trace: true,
                max_depth: 5,
            },
        )
        .await
        .unwrap();
    let trace = serde_json::to_value(response.trace.unwrap().as_graph().unwrap()).unwrap();
    let items = trace["allocate"]["traceData"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    // each entry is the sub-decision's trace; its input carries the state before that item
    assert_eq!(
        items[1]["consume"]["input"]["state"],
        json!({"remaining": 70})
    );
}

#[test]
fn sequential_content_round_trips_and_other_modes_serialise_as_before() {
    let content = budget_rule();
    let value = serde_json::to_value(&content).unwrap();
    let attributes = &value["nodes"][1]["content"];
    assert_eq!(attributes["executionMode"], json!("sequential"));
    assert_eq!(attributes["stateField"], json!("{remaining: budget}"));
    assert!(attributes.get("statePath").is_none());
    let again: DecisionContent = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&again).unwrap(), value);

    let looped = graph(vec![expressions(
        "x",
        &[("a", "1")],
        json!({"inputField": "items", "executionMode": "loop"}),
    )]);
    let value = serde_json::to_value(&looped).unwrap();
    let content = value["nodes"][1]["content"].as_object().unwrap();
    for key in ["stateField", "statePath", "maxIterations"] {
        assert!(!content.contains_key(key), "{key} in {value}");
    }
}
