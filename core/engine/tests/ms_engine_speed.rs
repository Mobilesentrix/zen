//! Engine speed changes P2 (input-node date walk) and P3 (expression results are copied only
//! when a later write could reach them). Both must leave every answer exactly as before:
//! `ms_expression_alias_golden.txt` was produced by the engine before the change (zen 615aa9b),
//! over a few hundred generated key sequences, and is compared line by line, traced and not.
use serde_json::{json, Value};
use std::sync::Arc;
use zen_engine::loader::{DynamicLoader, MemoryLoader};
use zen_engine::model::DecisionContent;
use zen_engine::{Decision, EvaluationOptions};

fn expression_items(prefix: &str, expressions: &[(&str, &str)]) -> Vec<Value> {
    expressions
        .iter()
        .enumerate()
        .map(
            |(i, (key, value))| json!({ "id": format!("{prefix}{i}"), "key": key, "value": value }),
        )
        .collect()
}

fn graph(expressions: &[(&str, &str)]) -> DecisionContent {
    serde_json::from_value(json!({
        "nodes": [
            { "id": "in", "type": "inputNode", "name": "Request", "position": {"x": 0, "y": 0} },
            { "id": "ex", "type": "expressionNode", "name": "ex", "position": {"x": 0, "y": 0},
              "content": { "expressions": expression_items("e", expressions) } },
            { "id": "out", "type": "outputNode", "name": "Response", "position": {"x": 0, "y": 0} }
        ],
        "edges": [
            { "id": "1", "sourceId": "in", "type": "edge", "targetId": "ex" },
            { "id": "2", "sourceId": "ex", "type": "edge", "targetId": "out" }
        ]
    }))
    .unwrap()
}

fn compiled(content: DecisionContent) -> zen_engine::model::GraphContent {
    let DecisionContent::Graph(graph) = content else {
        panic!("graph expected")
    };
    let mut graph = (*graph).clone();
    graph.compile();
    graph
}

async fn run(content: DecisionContent, input: Value, trace: bool) -> String {
    let decision = Decision::from(Arc::new(compiled(content)));
    render(
        decision
            .evaluate_with_opts(
                input.into(),
                EvaluationOptions {
                    trace,
                    max_depth: 5,
                },
            )
            .await,
    )
}

fn render(
    r: Result<zen_engine::DecisionGraphResponse, Box<zen_engine::EvaluationError>>,
) -> String {
    let text = match r {
        Ok(ok) => serde_json::to_string(&ok).unwrap(),
        Err(err) => format!("ERR {}", serde_json::to_string(&*err).unwrap()),
    };
    strip_performance(&text)
}

fn strip_performance(text: &str) -> String {
    const KEY: &str = r#""performance":""#;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(KEY) {
        out.push_str(&rest[..i + KEY.len()]);
        rest = &rest[i + KEY.len()..];
        let end = rest.find('"').unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

async fn result_of(expressions: &[(&str, &str)]) -> Value {
    let text = run(graph(expressions), json!({}), false).await;
    let whole: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{text}"));
    whole["result"].clone()
}

// ---- P3: aliasing cases, one per key shape --------------------------------------------------

#[tokio::test]
async fn write_to_the_source_does_not_reach_a_copy() {
    // `copy` reads `x`; the later key `x.y` writes into the object stored under `x`.
    let r = result_of(&[("x", "{'a': 1}"), ("copy", "$.x"), ("x.y", "2")]).await;
    assert_eq!(r, json!({ "x": {"a": 1, "y": 2}, "copy": {"a": 1} }));
}

#[tokio::test]
async fn write_to_the_copy_does_not_reach_the_source() {
    let r = result_of(&[("x", "{'a': {'b': 1}}"), ("copy", "$.x"), ("copy.a.z", "5")]).await;
    assert_eq!(
        r,
        json!({ "x": {"a": {"b": 1}}, "copy": {"a": {"b": 1, "z": 5}} })
    );
}

#[tokio::test]
async fn two_copies_of_one_source_stay_apart() {
    let r = result_of(&[
        ("x", "{'a': 1}"),
        ("p", "$.x"),
        ("q", "$.x"),
        ("x.z", "1"),
        ("p.w", "2"),
    ])
    .await;
    assert_eq!(
        r,
        json!({ "x": {"a": 1, "z": 1}, "p": {"a": 1, "w": 2}, "q": {"a": 1} })
    );
}

#[tokio::test]
async fn copy_of_a_nested_part_of_the_source() {
    let r = result_of(&[("x", "{'a': {'b': 1}}"), ("inner", "$.x.a"), ("x.a.c", "3")]).await;
    assert_eq!(
        r,
        json!({ "x": {"a": {"b": 1, "c": 3}}, "inner": {"b": 1} })
    );
}

#[tokio::test]
async fn new_object_around_a_source_part() {
    let r = result_of(&[("x", "{'a': 1}"), ("wrap", "{'inner': $.x}"), ("x.b", "2")]).await;
    assert_eq!(
        r,
        json!({ "x": {"a": 1, "b": 2}, "wrap": {"inner": {"a": 1}} })
    );
}

#[tokio::test]
async fn array_of_source_parts() {
    let r = result_of(&[("x", "{'a': 1}"), ("list", "[$.x, $.x]"), ("x.b", "2")]).await;
    assert_eq!(
        r,
        json!({ "x": {"a": 1, "b": 2}, "list": [{"a": 1}, {"a": 1}] })
    );
}

#[tokio::test]
async fn the_whole_dollar_object_is_a_snapshot() {
    // `all = $` holds the object `$`; the keys written after it must not appear in it.
    let r = result_of(&[("x", "1"), ("all", "$"), ("later", "2")]).await;
    assert_eq!(r, json!({ "x": 1, "all": {"x": 1}, "later": 2 }));
}

#[tokio::test]
async fn dollar_inside_a_new_object_is_a_snapshot() {
    let r = result_of(&[("x", "1"), ("box", "{'s': $}"), ("later", "2"), ("x2", "3")]).await;
    assert_eq!(
        r,
        json!({ "x": 1, "box": {"s": {"x": 1}}, "later": 2, "x2": 3 })
    );
}

#[tokio::test]
async fn a_key_written_twice_keeps_its_last_value_only() {
    let r = result_of(&[("a", "{'n': 1}"), ("b", "$.a"), ("b", "7"), ("c.d", "1")]).await;
    assert_eq!(r, json!({ "a": {"n": 1}, "b": 7, "c": {"d": 1} }));
}

#[tokio::test]
async fn a_key_rewritten_then_written_into() {
    let r = result_of(&[
        ("a", "{'n': 1}"),
        ("p", "$.a"),
        ("p", "{'z': 9}"),
        ("p.y", "2"),
    ])
    .await;
    assert_eq!(r, json!({ "a": {"n": 1}, "p": {"z": 9, "y": 2} }));
}

#[tokio::test]
async fn traced_values_follow_the_result() {
    let text = run(
        graph(&[("x", "{'a': 1}"), ("copy", "$.x"), ("x.y", "2")]),
        json!({}),
        true,
    )
    .await;
    let whole: Value = serde_json::from_str(&text).unwrap();
    let node = whole["trace"]
        .as_object()
        .unwrap()
        .values()
        .find(|t| t["name"] == "ex")
        .unwrap();
    // x's trace entry is the object the result holds, so it carries the later write.
    assert_eq!(node["traceData"]["x"]["result"], json!({"a": 1, "y": 2}));
    assert_eq!(node["traceData"]["copy"]["result"], json!({"a": 1}));
}

#[tokio::test]
async fn untouched_copies_share_the_value_of_their_source() {
    // The point of the change: with no write after it, `copy` is the same object as `x`.
    let decision = Decision::from(Arc::new(compiled(graph(&[
        ("x", "{'big': [1, 2, 3]}"),
        ("copy", "$.x"),
    ]))));
    let response = decision.evaluate(json!({}).into()).await.unwrap();
    let object = response.result.as_object().unwrap();
    let object = object.borrow();
    let x = object.get_str("x").unwrap().as_object().unwrap();
    let copy = object.get_str("copy").unwrap().as_object().unwrap();
    assert!(std::rc::Rc::ptr_eq(&x, &copy));
}

#[tokio::test]
async fn generated_key_sequences_answer_as_before() {
    let golden_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/ms_expression_alias_golden.txt"
    );
    let mut answers = Vec::new();
    for (i, case) in alias_cases().iter().enumerate() {
        let pairs: Vec<(&str, &str)> = case.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        for trace in [false, true] {
            let text = run(graph(&pairs), json!({"inp": {"k": [1, {"z": 2}]}}), trace).await;
            answers.push(format!("{i}/{trace}: {text}"));
        }
    }
    let joined = answers.join("\n");
    if std::env::var("MS_WRITE_GOLDEN").is_ok() {
        std::fs::write(golden_path, joined).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(golden_path).unwrap();
    for (line, expected) in joined.lines().zip(golden.lines()) {
        assert_eq!(line, expected);
    }
    assert_eq!(joined.lines().count(), golden.lines().count());
}

fn alias_cases() -> Vec<Vec<(String, String)>> {
    const KEYS: [&str; 9] = ["a", "b", "c", "a.x", "a.y", "b.x", "c.d.e", "a.x.z", "d"];
    const VALUES: [&str; 14] = [
        "{'n': 1, 'm': [1, 2]}",
        "$.a",
        "$.b",
        "$",
        "$.a.x",
        "{'k': $.a}",
        "[$.a, $.b]",
        "5",
        "$.c",
        "{'q': {'r': 1}}",
        "inp.k",
        "$.d",
        "{'s': $, 't': $.a}",
        "filter([$.a, $.b, 1], # != null)",
    ];
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    (0..260)
        .map(|_| {
            let len = 3 + next(5);
            (0..len)
                .map(|_| {
                    (
                        KEYS[next(KEYS.len())].to_string(),
                        VALUES[next(VALUES.len())].to_string(),
                    )
                })
                .collect()
        })
        .collect()
}

// ---- P2: dates reach a sub-rule as their source text --------------------------------------

fn parent_child(
    input_schema: Option<Value>,
    child_expressions: &[(&str, &str)],
    parent_expressions: &[(&str, &str)],
) -> (DynamicLoader, DecisionContent) {
    let mut input_node =
        json!({ "id": "in", "type": "inputNode", "name": "Request", "position": {"x": 0, "y": 0} });
    if let Some(schema) = input_schema {
        input_node["content"] = json!({ "schema": schema.to_string() });
    }
    let loader = MemoryLoader::default();
    loader.add("child", graph(child_expressions));
    let (middle, edge_source) = match parent_expressions.is_empty() {
        true => (Vec::new(), "in"),
        false => (
            vec![
                json!({ "id": "ex", "type": "expressionNode", "name": "ex", "position": {"x": 0, "y": 0},
              "content": { "expressions": expression_items("p", parent_expressions) } }),
            ],
            "ex",
        ),
    };
    let mut nodes = vec![input_node];
    nodes.extend(middle);
    nodes.push(
        json!({ "id": "sub", "type": "decisionNode", "name": "sub", "position": {"x": 0, "y": 0},
              "content": { "key": "child" } }),
    );
    nodes.push(json!({ "id": "out", "type": "outputNode", "name": "Response", "position": {"x": 0, "y": 0} }));
    let mut edges =
        vec![json!({ "id": "3", "sourceId": "sub", "type": "edge", "targetId": "out" })];
    if edge_source == "ex" {
        edges.push(json!({ "id": "1", "sourceId": "in", "type": "edge", "targetId": "ex" }));
    }
    edges.push(json!({ "id": "2", "sourceId": edge_source, "type": "edge", "targetId": "sub" }));
    let parent = serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap();
    (Arc::new(loader), parent)
}

async fn run_parent(loader: DynamicLoader, parent: DecisionContent, input: Value) -> Value {
    let decision = Decision::from(Arc::new(compiled(parent))).with_loader(loader);
    let text = render(decision.evaluate(input.into()).await);
    let whole: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{text}"));
    whole["result"].clone()
}

#[tokio::test]
async fn a_date_made_at_the_input_reaches_the_sub_rule_as_text() {
    // The root input node turns the declared date into an engine date value; the sub-rule has
    // no input schema, so its input node turns it back into the source text. Both input nodes
    // must walk, although the evaluation's own input held no date value.
    let schema =
        json!({"type": "object", "properties": {"when": {"type": "string", "format": "date"}}});
    let (loader, parent) = parent_child(
        Some(schema),
        &[
            ("seen", "when"),
            ("size", "len(when)"),
            ("text", "when + '!'"),
            ("kind", "type(when)"),
        ],
        &[],
    );
    let r = run_parent(loader, parent, json!({"when": "2026-01-01"})).await;
    assert_eq!(r["seen"], json!("2026-01-01"));
    assert_eq!(r["size"], json!(10));
    assert_eq!(r["text"], json!("2026-01-01!"));
    assert_eq!(r["kind"], json!("string"));
}

#[tokio::test]
async fn a_date_nested_in_the_input_reaches_the_sub_rule_as_text() {
    let schema = json!({"type": "object", "properties": {"rows": {"type": "array",
        "items": {"type": "object", "properties": {"d": {"type": "string", "format": "date"}}}}}});
    let (loader, parent) = parent_child(
        Some(schema),
        &[("seen", "rows[0].d"), ("kind", "type(rows[0].d)")],
        &[],
    );
    let r = run_parent(loader, parent, json!({"rows": [{"d": "2026-02-03"}]})).await;
    assert_eq!(r["seen"], json!("2026-02-03"));
    assert_eq!(r["kind"], json!("string"));
}

#[tokio::test]
async fn sub_rule_without_dates_answers_as_before() {
    let (loader, parent) = parent_child(
        None,
        &[("sum", "a + b"), ("items", "list")],
        &[("a", "1"), ("b", "n"), ("list", "[{'x': n}]")],
    );
    let r = run_parent(loader, parent, json!({"n": 41})).await;
    assert_eq!(r, json!({"sum": 42, "items": [{"x": 41}]}));
}
