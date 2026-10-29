//! MobileSentrix: a compiled decision (prepared graph, compiled switch conditions and input
//! fields, decision-table cells resolved to bytecode, `$nodes` built only for its readers) must
//! answer exactly like the same decision evaluated uncompiled, traced and untraced, including
//! errors.
use serde_json::json;
use std::sync::Arc;
use zen_engine::loader::DynamicLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{Decision, EvaluationOptions};

mod support;
use support::{create_fs_loader, load_test_data};

const GRAPHS: &[&str] = &[
    "$nodes-parent.json",
    "credit-analysis.json",
    "expression.json",
    "passthrough.json",
    "recursive-table1.json",
    "switch-node.json",
    "switch-performance.json",
    "switch-performance-2.json",
    "table-collect-columns.json",
    "table.json",
    "8k.json",
    "error-cyclic.json",
    "error-missing-input.json",
    "error-missing-output.json",
];

fn inputs() -> Vec<serde_json::Value> {
    vec![
        json!({}),
        json!({ "input": 5 }),
        json!({ "input": 15, "color": "red" }),
        json!({ "input": 25, "customer": { "tier": "gold", "age": 40, "income": 120000 } }),
        json!({ "input": -3, "value": "a", "flag": true, "items": [1, 2, 3] }),
        json!({ "amount": 900, "country": "US", "score": 720, "age": 33, "$nodes": { "x": 1 } }),
    ]
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

/// Blanks every `"performance":"..."` value (timings are the only legitimate difference).
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

#[tokio::test]
async fn compiled_decisions_answer_like_uncompiled_ones() {
    let loader: DynamicLoader = Arc::new(create_fs_loader());
    for name in GRAPHS {
        let plain = load_test_data(name);
        let mut compiled = plain.clone();
        compiled.compile();
        assert!(compiled.compiled_cache.is_some(), "{name} compiled");

        let plain = Decision::from(Arc::new(plain)).with_loader(loader.clone());
        let compiled = Decision::from(Arc::new(compiled)).with_loader(loader.clone());
        for input in inputs() {
            for trace in [false, true] {
                let options = EvaluationOptions {
                    trace,
                    max_depth: 10,
                };
                let a = render(
                    plain
                        .evaluate_with_opts(input.clone().into(), options)
                        .await,
                );
                let b = render(
                    compiled
                        .evaluate_with_opts(input.clone().into(), options)
                        .await,
                );
                assert_eq!(a, b, "{name} trace={trace} input={input}");
            }
        }
    }
}

#[tokio::test]
async fn compiled_content_from_json_round_trips() {
    // compile() never changes what the content serialises to (caches are skipped).
    for name in GRAPHS {
        let plain = load_test_data(name);
        let mut compiled = plain.clone();
        compiled.compile();
        let a = serde_json::to_string(&DecisionContent::from(plain)).unwrap();
        let b = serde_json::to_string(&DecisionContent::from(compiled)).unwrap();
        assert_eq!(a, b, "{name}");
    }
}
