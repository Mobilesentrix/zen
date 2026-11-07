//! Per-item cost of `executionMode: "sequential"` against `"loop"` on the same step.
//! Run in release: `cargo test --release -p zen-engine --features arbitrary_precision
//! --test ms_sequential_bench -- --ignored --nocapture`.

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::DecisionEngine;

const ITEMS: usize = 1_000;
const WARM_UP: usize = 20;
const RUNS: usize = 200;

fn graph(step: Value) -> DecisionContent {
    serde_json::from_value(json!({
        "nodes": [
            {"id": "in", "name": "in", "type": "inputNode", "content": {}},
            step,
            {"id": "out", "name": "out", "type": "outputNode", "content": {}}
        ],
        "edges": [
            {"id": "e1", "sourceId": "in", "targetId": "step"},
            {"id": "e2", "sourceId": "step", "targetId": "out"}
        ]
    }))
    .expect("graph")
}

fn expression_step(mode: &str, rows: &[(&str, &str)]) -> DecisionContent {
    let rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, (key, value))| json!({"id": format!("r{i}"), "key": key, "value": value}))
        .collect();
    graph(
        json!({"id": "step", "name": "step", "type": "expressionNode", "content": {
            "expressions": rows, "inputField": "items", "executionMode": mode,
            "outputPath": "results", "stateField": "{remaining: budget}"
        }}),
    )
}

fn table() -> DecisionContent {
    graph(
        json!({"id": "step", "name": "step", "type": "decisionTableNode", "content": {
            "hitPolicy": "first",
            "inputs": [{"id": "i1", "field": "need"}],
            "outputs": [{"id": "o1", "field": "take"}],
            "rules": [{"_id": "a", "i1": "> 5", "o1": "need - 5"}, {"_id": "b", "i1": "", "o1": "need"}]
        }}),
    )
}

fn sub_decision_step(mode: &str) -> DecisionContent {
    graph(
        json!({"id": "step", "name": "step", "type": "decisionNode", "content": {
            "key": "table", "inputField": "items", "executionMode": mode,
            "outputPath": "results", "stateField": "{remaining: budget}"
        }}),
    )
}

async fn per_item_micros(engine: &DecisionEngine, key: &str, input: &Value) -> f64 {
    for _ in 0..WARM_UP {
        engine.evaluate(key, input.clone().into()).await.expect(key);
    }
    let started = Instant::now();
    for _ in 0..RUNS {
        engine.evaluate(key, input.clone().into()).await.expect(key);
    }
    started.elapsed().as_secs_f64() * 1e6 / (RUNS * ITEMS) as f64
}

#[tokio::test]
#[ignore = "benchmark"]
async fn sequential_overhead_per_item() {
    let loader = MemoryLoader::default();
    let same = [("take", "need * 2")];
    loader.add("expr-loop", expression_step("loop", &same));
    loader.add("expr-seq", expression_step("sequential", &same));
    loader.add(
        "expr-seq-state",
        expression_step(
            "sequential",
            &[
                ("take", "min([need, state.remaining])"),
                (
                    "state.remaining",
                    "state.remaining - min([need, state.remaining])",
                ),
            ],
        ),
    );
    loader.add("table", table());
    loader.add("sub-loop", sub_decision_step("loop"));
    loader.add("sub-seq", sub_decision_step("sequential"));
    let engine = DecisionEngine::default().with_loader(Arc::new(loader));

    let items: Vec<Value> = (0..ITEMS)
        .map(|i| json!({"warehouseId": i, "need": (i % 17) as f64 + 0.5, "sku": format!("S{i}")}))
        .collect();
    let input = json!({"budget": 2500, "items": items});

    for key in [
        "expr-loop",
        "expr-seq",
        "expr-seq-state",
        "sub-loop",
        "sub-seq",
    ] {
        let micros = per_item_micros(&engine, key, &input).await;
        println!("{key:16} {micros:8.3} µs/item");
    }
}
