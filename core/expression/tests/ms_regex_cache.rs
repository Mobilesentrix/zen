//! `extract()` and `matches()` reuse compiled regexes per thread. These tests pin that the
//! results do not depend on whether a pattern was compiled before, that an invalid pattern fails
//! on every call, and that results stay correct after the cache is emptied.

use serde_json::{json, Value};
use zen_expression::Isolate;

fn run(isolate: &mut Isolate, expression: &str) -> Result<Value, String> {
    isolate
        .run_standard(expression)
        .map(|v| v.to_value())
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn repeated_patterns_give_the_same_results() {
    let mut isolate = Isolate::new();
    for _ in 0..3 {
        assert_eq!(
            run(&mut isolate, r#"extract("2026-11-08", "(\d+)-(\d+)-(\d+)")"#).unwrap(),
            json!(["2026-11-08", "2026", "11", "08"])
        );
        assert_eq!(
            run(&mut isolate, r#"extract("no digits", "(\d+)")"#).unwrap(),
            json!([])
        );
        assert_eq!(
            run(&mut isolate, r#"matches("SKU-123", "^SKU-\d+$")"#).unwrap(),
            json!(true)
        );
        assert_eq!(
            run(&mut isolate, r#"matches("sku-123", "^SKU-\d+$")"#).unwrap(),
            json!(false)
        );
    }
}

#[test]
fn invalid_pattern_fails_on_every_call() {
    let mut isolate = Isolate::new();
    for _ in 0..3 {
        assert!(run(&mut isolate, r#"matches("a", "(")"#).is_err());
        assert!(run(&mut isolate, r#"extract("a", "(")"#).is_err());
    }
}

#[test]
fn results_stay_correct_past_the_cache_capacity() {
    let mut isolate = Isolate::new();
    for round in 0..2 {
        for n in 0..600 {
            let expression = format!(r#"matches("item-{n}", "^item-{n}$")"#);
            assert_eq!(
                run(&mut isolate, &expression).unwrap(),
                json!(true),
                "round {round}, pattern {n}"
            );
            let expression = format!(r#"matches("item-{n}x", "^item-{n}$")"#);
            assert_eq!(
                run(&mut isolate, &expression).unwrap(),
                json!(false),
                "round {round}, pattern {n}"
            );
        }
    }
}

#[test]
fn threads_get_the_same_results() {
    let handles: Vec<_> = (0..4)
        .map(|t| {
            std::thread::spawn(move || {
                let mut isolate = Isolate::new();
                for n in 0..100 {
                    let expression = format!(r#"extract("t{t}-{n}", "t(\d+)-(\d+)")"#);
                    assert_eq!(
                        run(&mut isolate, &expression).unwrap(),
                        json!([format!("t{t}-{n}"), t.to_string(), n.to_string()])
                    );
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
}
