//! Cost of the expression list functions against the same work in a JavaScript Function node.
//!
//! Workload (the "order and number" and "budget consumed in order" rules): 200 rows, sorted
//! by two keys, and a running total in warehouse order.
//!
//! `cargo bench -p zen-engine --features arbitrary_precision --bench ms_list_vs_function`

use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::runtime::Runtime;
use zen_engine::loader::NoopLoader;
use zen_engine::model::DecisionContent;
use zen_engine::nodes::custom::NoopCustomNode;
use zen_engine::DecisionEngine;
use zen_expression::variable::Variable;
use zen_expression::Isolate;

const ROWS: usize = 200;

const SORTED: &str = "sortBy(rows, [#.listOrd, #.routeOrd])";
const RUNNING: &str = "scan(sortBy(rows, #.wid), acc + #.cost, 0)";

const JS: &str = r#"export const handler = async (input) => {
  const sorted = [...input.rows].sort((a, b) => a.listOrd - b.listOrd || a.routeOrd - b.routeOrd);
  const byWid = [...input.rows].sort((a, b) => a.wid - b.wid);
  let acc = 0;
  const running = byWid.map((r) => (acc += r.cost));
  return { sorted, running };
};"#;

fn rows() -> Value {
    let rows: Vec<Value> = (0..ROWS)
        .map(|i| {
            json!({
                "id": i,
                "wid": (i * 7919) % 13,
                "listOrd": (i * 104729) % 17,
                "routeOrd": (i * 31) % 5,
                "cost": ((i * 37) % 100) as f64 / 4.0,
            })
        })
        .collect();
    json!({ "rows": rows })
}

fn graph(middle: Value) -> Arc<DecisionContent> {
    let content = json!({
        "nodes": [
            { "id": "in", "name": "request", "type": "inputNode", "position": { "x": 0, "y": 0 } },
            middle,
            { "id": "out", "name": "response", "type": "outputNode", "position": { "x": 0, "y": 0 } }
        ],
        "edges": [
            { "id": "e1", "type": "edge", "sourceId": "in", "targetId": "work" },
            { "id": "e2", "type": "edge", "sourceId": "work", "targetId": "out" }
        ]
    });
    Arc::new(serde_json::from_value(content).unwrap())
}

fn expression_graph() -> Arc<DecisionContent> {
    graph(json!({
        "id": "work", "name": "work", "type": "expressionNode", "position": { "x": 0, "y": 0 },
        "content": { "expressions": [
            { "id": "a", "key": "sorted", "value": SORTED },
            { "id": "b", "key": "running", "value": RUNNING }
        ] }
    }))
}

fn function_graph() -> Arc<DecisionContent> {
    graph(json!({
        "id": "work", "name": "work", "type": "functionNode", "position": { "x": 0, "y": 0 },
        "content": { "source": JS }
    }))
}

fn bench(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let engine = DecisionEngine::new(Arc::new(NoopLoader), Arc::new(NoopCustomNode::default()));
    let context: Variable = rows().into();

    let mut group = c.benchmark_group("ms_list_200_rows");

    let mut isolate = Isolate::with_environment(context.clone());
    group.bench_function("isolate sortBy 2 keys", |b| {
        b.iter(|| criterion::black_box(isolate.run_standard(SORTED).unwrap()))
    });
    group.bench_function("isolate scan(sortBy) running total", |b| {
        b.iter(|| criterion::black_box(isolate.run_standard(RUNNING).unwrap()))
    });

    for (name, content) in [
        ("expression node", expression_graph()),
        ("function node (JS)", function_graph()),
    ] {
        let mut decision = engine.create_decision(content).unwrap();
        decision.compile();
        let check = rt.block_on(decision.evaluate(context.clone())).unwrap();
        assert_eq!(
            check.result.to_value()["running"]
                .as_array()
                .map(|a| a.len()),
            Some(ROWS),
            "{name} produced the wrong shape"
        );
        group.bench_function(name, |b| {
            b.to_async(&rt).iter(|| async {
                criterion::black_box(decision.evaluate(context.clone()).await.unwrap())
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
