//! MobileSentrix list functions: sort, sortBy, minBy, maxBy, unique, uniqueBy, countDistinct,
//! groupBy, keyBy, join, first, last, reduce, scan. Value cases are in `data/ms_list.csv`.

use anyhow::Context;
use serde_json::{json, Value};
use std::ops::Index;
use zen_expression::{Isolate, Variable};

fn run(expression: &str, input: Value) -> anyhow::Result<Value> {
    let mut isolate = Isolate::new();
    isolate.set_environment(input.into());
    isolate
        .run_standard(expression)
        .map(|v| v.to_value())
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

#[test]
fn ms_list_csv() {
    let csv_data = include_str!("data/ms_list.csv");
    let mut r = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(csv_data.as_bytes());

    let mut cases = 0;
    for row in r.records() {
        let Ok(row) = row else {
            continue;
        };

        let (expression, input_str, output_str) = (row.index(0), row.index(1), row.index(2));
        if expression.starts_with('#') {
            continue;
        }

        let output: Value = serde_json5::from_str(output_str)
            .with_context(|| format!("output of {expression}"))
            .unwrap();
        let input: Value = match input_str.is_empty() {
            true => json!({}),
            false => serde_json5::from_str(input_str)
                .with_context(|| format!("input of {expression}"))
                .unwrap(),
        };

        let result = run(expression, input)
            .with_context(|| format!("Expression: {expression}"))
            .unwrap();
        assert_eq!(
            result, output,
            "Expression {expression}. Expected: {output}, got: {result}"
        );
        cases += 1;
    }

    assert!(cases > 90, "only {cases} cases ran");
}

#[test]
fn ms_list_errors() {
    let cases: &[(&str, Value, &str)] = &[
        (
            "sort([1, 'a'])",
            json!({}),
            "cannot compare number keys with string keys",
        ),
        (
            "sortBy(xs, #.k)",
            json!({"xs": [{"k": 1}, {"k": "1"}]}),
            "sortBy: cannot compare number keys with string keys",
        ),
        (
            "sortBy(xs, #.k)",
            json!({"xs": [{"k": true}, {"k": null}, {"k": 2}]}),
            "cannot compare bool keys with number keys",
        ),
        (
            "sortBy(xs, #)",
            json!({"xs": [{"a": 1}]}),
            "objects cannot be sort keys",
        ),
        (
            "minBy(xs, #)",
            json!({"xs": [{"a": 1}]}),
            "minBy: objects cannot be sort keys",
        ),
        (
            "sortBy([1, 2], #, 'up')",
            json!({}),
            "direction must be 'asc' or 'desc'",
        ),
        (
            "sort([1, 2], 'down')",
            json!({}),
            "direction must be 'asc' or 'desc'",
        ),
        (
            "sortBy(xs, [#.a, #.b])",
            json!({"xs": [{"a": 1, "b": 1}, {"a": "x", "b": 2}]}),
            "key part 0 mixes number and string values",
        ),
        (
            "sortBy(xs, #.k)",
            json!({"xs": [{"k": [1]}, {"k": 2}]}),
            "keys mix arrays and number values",
        ),
        (
            "sort([[[1]], [[2]]])",
            json!({}),
            "nested arrays cannot be sort keys",
        ),
        ("join([1, null], ',')", json!({}), "join: element 1 is null"),
        (
            "join(['a', ['b']], ',')",
            json!({}),
            "join: element 1 is array",
        ),
        (
            "groupBy(xs, #)",
            json!({"xs": [{"a": 1}]}),
            "groupBy: key must be a string, number, bool, date or null, got object",
        ),
        ("keyBy([[1]], #)", json!({}), "keyBy: key must be"),
        ("sortBy(1, #)", json!({}), "Unsupported type"),
        ("reduce(5, acc + #, 0)", json!({}), "Unsupported type"),
        // Unchanged: `min` / `max` of an empty array stay an error, like `avg` / `median`.
        ("min([])", json!({}), "Empty array"),
        ("max([])", json!({}), "Empty array"),
        ("first(5)", json!({}), ""),
    ];

    for (expression, input, needle) in cases {
        let err = match run(expression, input.clone()) {
            Ok(v) => panic!("{expression}: expected an error, got {v}"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            err.contains(needle),
            "{expression}: error does not contain {needle:?}: {err}"
        );
    }
}

#[test]
fn ms_list_wrong_argument_counts_fail_to_compile() {
    for expression in [
        "map([1], #, 1)",
        "filter([1], #, 1)",
        "sortBy([1], #, 'asc', 1)",
        "minBy([1], #, 1)",
        "reduce([1], acc + #)",
        "scan([1], acc + #)",
        "reduce([1], acc + #, 0, 1)",
        "sort()",
        "join([1])",
    ] {
        let result = run(expression, json!({}));
        assert!(
            result.is_err(),
            "{expression}: expected an error, got {result:?}"
        );
    }
}

/// `acc` is only special inside a reduce / scan callback; elsewhere it reads the context.
#[test]
fn ms_list_accumulator_scope() {
    assert_eq!(
        run("map([1, 2], acc + #)", json!({"acc": 10})).unwrap(),
        json!([11, 12])
    );
    // An alias named `acc` shadows the accumulator in its own closure.
    assert_eq!(
        run("reduce([1, 2] as acc, acc * 10, 0)", json!({})).unwrap(),
        json!(20)
    );
    // The inner reduce reads its own accumulator; the outer `acc` is read outside it.
    assert_eq!(
        run(
            "reduce([[1, 2], [3]] as row, acc + reduce(row, acc + # * 10, 0), 1)",
            json!({})
        )
        .unwrap(),
        json!(61)
    );
}

/// Numbers stay decimal through the folds.
#[test]
fn ms_list_decimal_arithmetic() {
    let result = run(
        "reduce(xs, acc + #, 0)",
        json!({"xs": [0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1]}),
    )
    .unwrap();
    assert_eq!(result, json!(1));

    let mut isolate = Isolate::new();
    let v = isolate
        .run_standard("last(scan([0.1, 0.2, 0.3], acc + #, 0))")
        .unwrap();
    assert_eq!(v.to_string(), "0.6");
}

/// The source array and the context are not mutated by sorting.
#[test]
fn ms_list_does_not_mutate_input() {
    let mut isolate = Isolate::new();
    let env: Variable = json!({"xs": [3, 1, 2]}).into();
    isolate.set_environment(env.clone());
    let sorted = isolate.run_standard("sortBy(xs, #)").unwrap();
    assert_eq!(sorted.to_value(), json!([1, 2, 3]));
    assert_eq!(env.to_value(), json!({"xs": [3, 1, 2]}));
}

/// The compiled bytecode is reusable: a second run of the same expression gives the same result.
#[test]
fn ms_list_repeat_runs() {
    let mut isolate = Isolate::new();
    isolate.set_environment(json!({"xs": [{"q": 2}, {"q": 1}]}).into());
    for _ in 0..3 {
        let v = isolate
            .run_standard(
                "[map(sortBy(xs, #.q), #.q), reduce(xs, acc + #.q, 0), scan(xs, acc + #.q, 0)]",
            )
            .unwrap();
        assert_eq!(v.to_value(), json!([[1, 2], 3, [2, 3]]));
    }
}

/// Unary (decision-table cell) mode: `$` is the cell's input.
#[test]
fn ms_list_unary() {
    let mut isolate = Isolate::new();
    isolate.set_environment(json!({"$": 3, "xs": [{"p": 3}, {"p": 1}]}).into());
    assert!(isolate.run_unary("maxBy(xs, #.p).p").unwrap());
    assert!(isolate.run_unary("sortBy(map(xs, #.p), #)").unwrap());
    assert!(!isolate.run_unary("minBy(xs, #.p).p").unwrap());
}
