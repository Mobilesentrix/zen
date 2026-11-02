//! MobileSentrix: database steps that are ready together run together. A decision must answer
//! exactly as it does when its steps run one by one (result, trace and errors, timings aside),
//! and the steps' waits must overlap.
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zen_engine::model::{DecisionContent, GraphContent};
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest};
use zen_engine::nodes::{NodeError, NodeResponse, NodeResult};
use zen_engine::{Decision, EvaluationOptions};

/// A stand-in for the database step: waits `delayMs`, then answers with what it was given
/// (minus `$nodes`, which the real step strips too), or fails when `fail` is set.
#[derive(Debug, Default)]
struct SlowDatabase {
    runs: Mutex<Vec<(String, Instant, Instant)>>,
    /// Steps whose handler was entered (recorded before the wait).
    started: Mutex<Vec<String>>,
}

impl CustomNodeAdapter for SlowDatabase {
    fn handle(&self, request: CustomNodeRequest) -> Pin<Box<dyn Future<Output = NodeResult> + '_>> {
        Box::pin(async move {
            let config: &Value = &request.node.config;
            self.started
                .lock()
                .unwrap()
                .push(request.node.name.to_string());
            let start = Instant::now();
            tokio::time::sleep(Duration::from_millis(
                config["delayMs"].as_u64().unwrap_or(0),
            ))
            .await;
            self.runs
                .lock()
                .unwrap()
                .push((request.node.name.to_string(), start, Instant::now()));
            if config["fail"].as_bool() == Some(true) {
                return Err(NodeError {
                    node_id: request.node.id.clone(),
                    trace: Some(json!({"failed": request.node.name.to_string()}).into()),
                    source: format!("{} failed", request.node.name).into(),
                });
            }
            let mut input: Value = serde_json::to_value(&request.input).unwrap();
            if let Value::Object(map) = &mut input {
                map.remove("$nodes");
            }
            let mut output = serde_json::Map::new();
            output.insert(
                request.node.name.to_string(),
                json!({ "rows": [input["store"].clone()], "n": config["delayMs"] }),
            );
            Ok(NodeResponse {
                output: Value::Object(output).into(),
                trace_data: Some(json!({ "sql": format!("select {}", request.node.name) }).into()),
            })
        })
    }
}

fn db(id: &str, delay: u64, fail: bool, sequential: bool) -> Value {
    let mut config = json!({ "query": "SELECT 1", "delayMs": delay, "fail": fail });
    if sequential {
        // Naming `$nodes` keeps a step out of the concurrent set (the adapter ignores it).
        config["note"] = json!("$nodes");
    }
    json!({ "id": id, "name": id, "type": "customNode", "content": { "kind": "database", "config": config } })
}

fn edge(id: &str, from: &str, to: &str) -> Value {
    json!({ "id": id, "sourceId": from, "targetId": to, "type": "edge" })
}

/// request -> read (expression) -> {stock, store, prices} -> merge (expression, reads all
/// three and `$nodes`) -> response. The steps finish in a different order than they start.
fn graph(fail: Option<&str>, sequential: bool) -> GraphContent {
    let steps = [("stock", 60), ("store", 30), ("prices", 45)];
    let mut nodes = vec![
        json!({ "id": "req", "name": "request", "type": "inputNode", "content": {} }),
        json!({ "id": "read", "name": "read", "type": "expressionNode", "content": { "expressions": [
            { "id": "e1", "key": "store", "value": "store ?? 'main'" }] } }),
        json!({ "id": "merge", "name": "merge", "type": "expressionNode", "content": { "expressions": [
            { "id": "e2", "key": "total", "value": "len(stock.rows) + len(store.rows) + len(prices.rows)" },
            { "id": "e3", "key": "names", "value": "keys($nodes)" }] } }),
        json!({ "id": "out", "name": "response", "type": "outputNode", "content": {} }),
    ];
    let mut edges = vec![edge("x1", "req", "read"), edge("x2", "merge", "out")];
    for (i, (name, delay)) in steps.iter().enumerate() {
        nodes.push(db(name, *delay, fail == Some(name), sequential));
        edges.push(edge(&format!("a{i}"), "read", name));
        edges.push(edge(&format!("b{i}"), name, "merge"));
    }
    let content: DecisionContent =
        serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap();
    let DecisionContent::Graph(g) = content else {
        panic!("graph")
    };
    let mut g = (*g).clone();
    g.compile();
    g
}

fn render(
    r: Result<zen_engine::DecisionGraphResponse, Box<zen_engine::EvaluationError>>,
) -> String {
    let text = match r {
        Ok(ok) => serde_json::to_string(&ok).unwrap(),
        Err(err) => format!("ERR {}", serde_json::to_string(&*err).unwrap()),
    };
    let mut v: Value = serde_json::from_str(text.trim_start_matches("ERR ")).unwrap();
    blank_performance(&mut v);
    format!(
        "{}{}",
        if text.starts_with("ERR ") { "ERR " } else { "" },
        v
    )
}

fn blank_performance(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.contains_key("performance") {
                map.insert("performance".into(), json!(""));
            }
            map.values_mut().for_each(blank_performance);
        }
        Value::Array(items) => items.iter_mut().for_each(blank_performance),
        _ => {}
    }
}

async fn run(
    fail: Option<&str>,
    sequential: bool,
    trace: bool,
) -> (String, Vec<(String, Instant, Instant)>) {
    let adapter = Arc::new(SlowDatabase::default());
    let decision = Decision::from(Arc::new(graph(fail, sequential))).with_adapter(adapter.clone());
    let out = decision
        .evaluate_with_opts(
            json!({ "store": "north" }).into(),
            EvaluationOptions {
                trace,
                max_depth: 10,
            },
        )
        .await;
    let runs = adapter.runs.lock().unwrap().clone();
    (render(out), runs)
}

fn overlapped(runs: &[(String, Instant, Instant)]) -> bool {
    runs.iter()
        .enumerate()
        .any(|(i, a)| runs.iter().skip(i + 1).any(|b| a.1 < b.2 && b.1 < a.2))
}

#[tokio::test]
async fn concurrent_steps_answer_like_sequential_ones_and_overlap() {
    for trace in [false, true] {
        let (sequential, seq_runs) = run(None, true, trace).await;
        let (concurrent, con_runs) = run(None, false, trace).await;
        assert_eq!(seq_runs.len(), 3);
        assert_eq!(con_runs.len(), 3);
        assert!(
            !overlapped(&seq_runs),
            "sequential reference must not overlap"
        );
        assert!(overlapped(&con_runs), "ready database steps must overlap");
        assert_eq!(sequential, concurrent, "trace={trace}");
        assert!(concurrent.contains(r#""total":3"#), "{concurrent}");
    }
}

#[tokio::test]
async fn first_failing_step_in_walk_order_wins() {
    for fail in ["stock", "store", "prices"] {
        for trace in [false, true] {
            let (sequential, _) = run(Some(fail), true, trace).await;
            let (concurrent, _) = run(Some(fail), false, trace).await;
            assert!(sequential.starts_with("ERR "), "{sequential}");
            assert_eq!(sequential, concurrent, "fail={fail} trace={trace}");
        }
    }
}

/// The PLP shape: request -> read -> { store -> site } and { plan (expression) -> catalog },
/// joined by a step that reads both. Walk order is store, site, plan, catalog: the catalog
/// lookup can only overlap the store/site chain if `plan` is computed ahead of the walk.
fn two_chains(sequential: bool, with_switch: bool) -> GraphContent {
    let mut nodes = vec![
        json!({ "id": "req", "name": "request", "type": "inputNode", "content": {} }),
        json!({ "id": "read", "name": "read", "type": "expressionNode", "content": { "expressions": [
            { "id": "e1", "key": "store", "value": "store ?? 'main'" }] } }),
        db("store", 20, false, sequential),
        db("site", 40, false, sequential),
        json!({ "id": "plan", "name": "plan", "type": "expressionNode", "content": { "expressions": [
            { "id": "e2", "key": "plan", "value": "[1, 2, 3]" }] } }),
        db("catalog", 70, false, sequential),
        json!({ "id": "join", "name": "join", "type": "expressionNode", "content": { "expressions": [
            { "id": "e3", "key": "total", "value": "len(site.rows) + len(catalog.rows)" }] } }),
        json!({ "id": "out", "name": "response", "type": "outputNode", "content": {} }),
    ];
    let mut edges = vec![
        edge("x1", "req", "read"),
        edge("x2", "store", "site"),
        edge("x3", "read", "plan"),
        edge("x4", "plan", "catalog"),
        edge("x5", "site", "join"),
        edge("x6", "catalog", "join"),
        edge("x7", "join", "out"),
    ];
    if with_switch {
        // read -> gate: the "skip" branch (an extra lookup) is never taken.
        nodes.push(json!({ "id": "gate", "name": "gate", "type": "switchNode", "content": { "hitPolicy": "first", "statements": [
            { "id": "take", "condition": "store == 'north'" }, { "id": "skip", "condition": "" }] } }));
        nodes.push(db("unused", 5, true, sequential));
        edges.push(edge("s1", "read", "gate"));
        edges.push(json!({ "id": "s2", "sourceId": "gate", "targetId": "store", "type": "edge", "sourceHandle": "take" }));
        edges.push(json!({ "id": "s3", "sourceId": "gate", "targetId": "unused", "type": "edge", "sourceHandle": "skip" }));
        edges.push(edge("s4", "unused", "join"));
    } else {
        edges.push(edge("x8", "read", "store"));
    }
    let content: DecisionContent =
        serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap();
    let DecisionContent::Graph(g) = content else {
        panic!("graph")
    };
    let mut g = (*g).clone();
    g.compile();
    g
}

async fn run_two_chains(
    sequential: bool,
    with_switch: bool,
    trace: bool,
) -> (String, Vec<(String, Instant, Instant)>, Duration) {
    let adapter = Arc::new(SlowDatabase::default());
    let decision =
        Decision::from(Arc::new(two_chains(sequential, with_switch))).with_adapter(adapter.clone());
    let t = Instant::now();
    let out = decision
        .evaluate_with_opts(
            json!({ "store": "north" }).into(),
            EvaluationOptions {
                trace,
                max_depth: 10,
            },
        )
        .await;
    let wall = t.elapsed();
    let runs = adapter.runs.lock().unwrap().clone();
    (render(out), runs, wall)
}

#[tokio::test]
async fn chains_overlap_when_a_pure_node_feeds_the_second_one() {
    for trace in [false, true] {
        let (sequential, seq_runs, seq_wall) = run_two_chains(true, false, trace).await;
        let (concurrent, con_runs, con_wall) = run_two_chains(false, false, trace).await;
        assert_eq!(sequential, concurrent, "trace={trace}");
        assert!(concurrent.contains(r#""total":2"#), "{concurrent}");
        assert!(!overlapped(&seq_runs));
        assert!(
            overlapped(&con_runs),
            "catalog must run alongside store/site"
        );
        // 20 + 40 + 70 one after another vs max(20 + 40, 70) side by side.
        assert!(seq_wall >= Duration::from_millis(130), "{seq_wall:?}");
        assert!(con_wall < Duration::from_millis(120), "{con_wall:?}");
    }
}

#[tokio::test]
async fn a_branch_a_switch_drops_never_runs_or_records() {
    for trace in [false, true] {
        let (sequential, seq_runs, _) = run_two_chains(true, true, trace).await;
        let (concurrent, con_runs, _) = run_two_chains(false, true, trace).await;
        assert_eq!(sequential, concurrent, "trace={trace}");
        assert!(!sequential.starts_with("ERR "), "{sequential}");
        for runs in [&seq_runs, &con_runs] {
            assert!(runs.iter().all(|(name, ..)| name != "unused"), "{runs:?}");
        }
    }
}

/// request -> read -> { lookup (a concurrent database step), audit (a step that names `$nodes`,
/// so it runs in walk order) } -> join -> response. Only one step may start early. When the walk
/// takes `audit` first, the lookup started early waits beside it: starting early pays whenever a
/// step has work beside it, not only when two concurrent steps are ready together. When the walk
/// takes `lookup` first it simply waits for it. `audit_first` swaps the edge order.
fn one_early_step(sequential: bool, audit_first: bool) -> GraphContent {
    let nodes = vec![
        json!({ "id": "req", "name": "request", "type": "inputNode", "content": {} }),
        json!({ "id": "read", "name": "read", "type": "expressionNode", "content": { "expressions": [
            { "id": "e1", "key": "store", "value": "store ?? 'main'" }] } }),
        db("lookup", 60, false, sequential),
        db("audit", 60, false, true),
        json!({ "id": "join", "name": "join", "type": "expressionNode", "content": { "expressions": [
            { "id": "e3", "key": "total", "value": "len(lookup.rows) + len(audit.rows)" }] } }),
        json!({ "id": "out", "name": "response", "type": "outputNode", "content": {} }),
    ];
    let mut branches = vec![edge("x2", "read", "lookup"), edge("x3", "read", "audit")];
    if audit_first {
        branches.reverse();
    }
    let mut edges = vec![edge("x1", "req", "read")];
    edges.extend(branches);
    edges.extend([
        edge("x4", "lookup", "join"),
        edge("x5", "audit", "join"),
        edge("x6", "join", "out"),
    ]);
    let content: DecisionContent =
        serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap();
    let DecisionContent::Graph(g) = content else {
        panic!("graph")
    };
    let mut g = (*g).clone();
    g.compile();
    g
}

async fn run_one_early_step(
    sequential: bool,
    audit_first: bool,
    trace: bool,
) -> (String, Vec<(String, Instant, Instant)>) {
    let adapter = Arc::new(SlowDatabase::default());
    let decision = Decision::from(Arc::new(one_early_step(sequential, audit_first)))
        .with_adapter(adapter.clone());
    let out = decision
        .evaluate_with_opts(
            json!({ "store": "north" }).into(),
            EvaluationOptions {
                trace,
                max_depth: 10,
            },
        )
        .await;
    let runs = adapter.runs.lock().unwrap().clone();
    (render(out), runs)
}

#[tokio::test]
async fn a_single_early_step_overlaps_the_work_the_walk_runs_before_it() {
    let mut audit_walked_first = false;
    for audit_first in [false, true] {
        for trace in [false, true] {
            let (sequential, seq_runs) = run_one_early_step(true, audit_first, trace).await;
            let (concurrent, con_runs) = run_one_early_step(false, audit_first, trace).await;
            assert_eq!(
                sequential, concurrent,
                "audit_first={audit_first} trace={trace}"
            );
            assert!(concurrent.contains(r#""total":2"#), "{concurrent}");
            assert!(!overlapped(&seq_runs));
            // The sequential runs record the walk order.
            if seq_runs[0].0 == "audit" {
                audit_walked_first = true;
                assert!(
                    overlapped(&con_runs),
                    "the lookup must wait beside the audit step"
                );
            }
        }
    }
    assert!(
        audit_walked_first,
        "one of the two edge orders walks audit first"
    );
}

/// request -> read -> { check (a step that fails after 30 ms; it names `$nodes`, so it runs in
/// walk order), step (a database step) } -> join -> response, with the walk taking `check`
/// first. A read-only step starts beside `check`; a step that writes must never run ahead of
/// the walk, so the failure leaves it untouched.
fn step_beside_a_failing_node(query: &str) -> GraphContent {
    let nodes = vec![
        json!({ "id": "req", "name": "request", "type": "inputNode", "content": {} }),
        json!({ "id": "read", "name": "read", "type": "expressionNode", "content": { "expressions": [
            { "id": "e1", "key": "store", "value": "store ?? 'main'" }] } }),
        db("check", 30, true, true),
        json!({ "id": "step", "name": "step", "type": "customNode", "content": { "kind": "database",
            "config": { "query": query, "delayMs": 30 } } }),
        json!({ "id": "join", "name": "join", "type": "expressionNode", "content": { "expressions": [
            { "id": "e3", "key": "total", "value": "len(step.rows)" }] } }),
        json!({ "id": "out", "name": "response", "type": "outputNode", "content": {} }),
    ];
    let edges = vec![
        edge("x1", "req", "read"),
        edge("x2", "read", "check"),
        edge("x3", "read", "step"),
        edge("x4", "check", "join"),
        edge("x5", "step", "join"),
        edge("x6", "join", "out"),
    ];
    let content: DecisionContent =
        serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap();
    let DecisionContent::Graph(g) = content else {
        panic!("graph")
    };
    let mut g = (*g).clone();
    g.compile();
    g
}

#[tokio::test]
async fn a_writing_step_never_runs_ahead_of_a_failing_node() {
    for (query, may_start) in [
        ("SELECT sku FROM products WHERE store = {{ store }}", true),
        ("INSERT INTO audit (store) VALUES ({{ store }})", false),
    ] {
        let adapter = Arc::new(SlowDatabase::default());
        let decision = Decision::from(Arc::new(step_beside_a_failing_node(query)))
            .with_adapter(adapter.clone());
        let out = decision
            .evaluate_with_opts(
                json!({ "store": "north" }).into(),
                EvaluationOptions {
                    trace: false,
                    max_depth: 10,
                },
            )
            .await;
        assert!(out.is_err(), "{query}: the check fails");
        let mut started = adapter.started.lock().unwrap().clone();
        started.sort();
        match may_start {
            true => assert_eq!(started, vec!["check", "step"], "{query}: starts early"),
            false => assert_eq!(started, vec!["check"], "{query}: must not run"),
        }
    }
}
