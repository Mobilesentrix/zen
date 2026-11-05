//! `{{#if}} … {{else}} … {{/if}}` optional SQL blocks and LIMIT / OFFSET counts (no database).

use ms_zen_database::{
    bind, BindError, BindValue, Dialect, NodeLabel, QueryTemplate, SlotKind, TemplateError,
};
use serde_json::{json, Value};
use zen_expression::Variable;

const NODE: NodeLabel<'static> = NodeLabel {
    name: "search",
    id: "db1",
};

fn parse(sql: &str, dialect: Dialect) -> QueryTemplate {
    QueryTemplate::parse(sql, dialect).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn run(t: &QueryTemplate, input: Value) -> Result<(String, Vec<BindValue>), BindError> {
    bind(t, &Variable::from(input), NODE).map(|b| (b.sql.to_string(), b.params))
}

fn ok(t: &QueryTemplate, input: Value) -> (String, Vec<BindValue>) {
    run(t, input.clone()).unwrap_or_else(|e| panic!("{input}: {e}"))
}

fn text(s: &str) -> BindValue {
    BindValue::Text(s.to_string())
}

fn squash(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

const SEARCH: &str = "SELECT sku FROM p WHERE status = 'active' \
    {{#if colour}} AND colour = {{ colour }} {{/if}} \
    {{#if brands}} AND brand IN ({{ brands }}) {{/if}} \
    ORDER BY rank DESC LIMIT {{ size ?? 24 }} OFFSET {{ (page ?? 0) * (size ?? 24) }}";

#[test]
fn a_filter_the_request_does_not_ask_for_is_dropped() {
    let t = parse(SEARCH, Dialect::MySql);
    assert!(t.is_dynamic());
    let (sql, params) = ok(&t, json!({}));
    assert_eq!(
        squash(&sql),
        "SELECT sku FROM p WHERE status = 'active' ORDER BY rank DESC LIMIT 24 OFFSET 0"
    );
    assert!(params.is_empty());
    let (sql, params) = ok(
        &t,
        json!({"colour": "black", "brands": ["Apple", "Samsung"], "page": 2, "size": 10}),
    );
    assert_eq!(
        squash(&sql),
        "SELECT sku FROM p WHERE status = 'active' AND colour = ? AND brand IN (?, ?) ORDER BY rank DESC LIMIT 10 OFFSET 20"
    );
    assert_eq!(params, vec![text("black"), text("Apple"), text("Samsung")]);
}

#[test]
fn not_asked_takes_every_empty_form() {
    let t = parse(SEARCH, Dialect::MySql);
    for absent in [
        json!(null),
        json!(""),
        json!("   "),
        json!([]),
        json!(false),
        json!(0),
        json!({}),
    ] {
        let (sql, params) = ok(&t, json!({"colour": absent, "brands": absent}));
        assert!(
            !sql.contains("colour =") && !sql.contains("IN ("),
            "{absent}: {sql}"
        );
        assert!(params.is_empty());
    }
}

#[test]
fn a_dropped_branch_is_never_evaluated() {
    // the placeholder would fail (null, and a failing function) if it were evaluated
    let t = parse(
        "SELECT 1 FROM t WHERE a = 1 {{#if model}} AND m = {{ model.name }} AND x = {{ date(model.when) }} {{/if}}",
        Dialect::MySql,
    );
    let (sql, params) = ok(&t, json!({"model": null}));
    assert_eq!(squash(&sql), "SELECT 1 FROM t WHERE a = 1");
    assert!(params.is_empty());
}

#[test]
fn a_kept_branch_still_refuses_a_null_value() {
    let t = parse(
        "SELECT 1 FROM t WHERE 1 = 1 {{#if on}} AND m = {{ model }} {{/if}}",
        Dialect::MySql,
    );
    let e = run(&t, json!({"on": true})).unwrap_err().to_string();
    assert!(e.contains("resolved to null"), "{e}");
}

#[test]
fn guards_combine_with_and_or_not() {
    let t = parse(
        "SELECT 1 FROM t WHERE 1 = 1 \
         {{#if model or brands}} AND fits {{/if}} \
         {{#if inStock and not hideStock}} AND stocked {{/if}} \
         {{#if len(brands ?? []) > 1 or (x == 1 and y == 2)}} AND many {{/if}}",
        Dialect::MySql,
    );
    let cases = [
        (json!({}), ""),
        (json!({"model": "iPhone 16"}), "AND fits"),
        (json!({"brands": ["A"]}), "AND fits"),
        (json!({"brands": ["A", "B"]}), "AND fits AND many"),
        (json!({"inStock": true}), "AND stocked"),
        (json!({"inStock": true, "hideStock": true}), ""),
        (json!({"x": 1, "y": 2}), "AND many"),
        (json!({"x": 1, "y": 3}), ""),
    ];
    for (input, expected) in cases {
        let (sql, _) = ok(&t, input.clone());
        assert_eq!(
            squash(&sql),
            squash(&format!("SELECT 1 FROM t WHERE 1 = 1 {expected}")),
            "{input}"
        );
    }
}

#[test]
fn else_picks_a_whitelisted_alternative() {
    // sort order from the request without ever putting request text into the SQL
    let t = parse(
        "SELECT sku FROM p ORDER BY {{#if sort == 'price'}} price ASC {{else}} {{#if sort == 'new'}} created DESC {{else}} rank DESC {{/if}} {{/if}}",
        Dialect::MySql,
    );
    for (sort, order) in [
        ("price", "price ASC"),
        ("new", "created DESC"),
        ("x'; DROP TABLE p; --", "rank DESC"),
    ] {
        let (sql, params) = ok(&t, json!({"sort": sort}));
        assert_eq!(squash(&sql), format!("SELECT sku FROM p ORDER BY {order}"));
        assert!(params.is_empty());
    }
}

#[test]
fn nested_blocks_and_postgres_markers_number_in_order() {
    let t = parse(
        "SELECT 1 FROM t WHERE a = {{ a }} {{#if b}} AND b = {{ b }} {{#if c}} AND c IN ({{ c }}) {{/if}} {{/if}} AND d = {{ d }}",
        Dialect::Postgres,
    );
    let (sql, params) = ok(&t, json!({"a": 1, "b": 2, "c": [3, 4], "d": 5}));
    assert_eq!(
        squash(&sql),
        "SELECT 1 FROM t WHERE a = $1 AND b = $2 AND c IN ($3, $4) AND d = $5"
    );
    assert_eq!(params.len(), 5);
    let (sql, _) = ok(&t, json!({"a": 1, "c": [3], "d": 5}));
    assert_eq!(squash(&sql), "SELECT 1 FROM t WHERE a = $1 AND d = $2");
}

#[test]
fn counts_are_checked_whole_numbers() {
    let t = parse("SELECT 1 FROM t LIMIT {{ n }}", Dialect::MySql);
    assert_eq!(t.placeholders()[0].kind, SlotKind::Count);
    assert_eq!(ok(&t, json!({"n": 5})).0, "SELECT 1 FROM t LIMIT 5");
    assert_eq!(ok(&t, json!({"n": "12"})).0, "SELECT 1 FROM t LIMIT 12");
    assert_eq!(ok(&t, json!({"n": 3.0})).0, "SELECT 1 FROM t LIMIT 3");
    for bad in [
        json!(-1),
        json!(2.5),
        json!("10; DROP TABLE t"),
        json!("1e3"),
        json!([1]),
        json!(true),
        json!(1_000_000_001u64),
    ] {
        let e = run(&t, json!({"n": bad})).unwrap_err().to_string();
        assert!(e.contains("whole number"), "{bad}: {e}");
    }
    let e = run(&t, json!({})).unwrap_err().to_string();
    assert!(e.contains("resolved to null"), "{e}");
    // MySQL `LIMIT offset, count` and both clauses
    let t = parse("SELECT 1 FROM t LIMIT {{ a }}, {{ b }}", Dialect::MySql);
    assert_eq!(
        ok(&t, json!({"a": 10, "b": 5})).0,
        "SELECT 1 FROM t LIMIT 10, 5"
    );
    // a placeholder after the LIMIT clause is a value again
    let t = parse(
        "SELECT * FROM (SELECT x FROM t LIMIT {{ n }}) s WHERE x = {{ x }}",
        Dialect::Postgres,
    );
    let (sql, params) = ok(&t, json!({"n": 3, "x": "a"}));
    assert_eq!(
        sql,
        "SELECT * FROM (SELECT x FROM t LIMIT 3) s WHERE x = $1"
    );
    assert_eq!(params, vec![text("a")]);
}

#[test]
fn templates_without_the_new_syntax_stay_static() {
    let t = parse(
        "SELECT a FROM t WHERE id IN ({{ ids }}) AND s = {{ s }}",
        Dialect::MySql,
    );
    assert!(!t.is_dynamic());
    assert!(t.guards().is_empty());
    assert!(t.placeholders().iter().all(|p| p.kind == SlotKind::Value));
}

#[test]
fn block_markers_are_checked_when_parsed() {
    let e = |sql: &str| {
        QueryTemplate::parse(sql, Dialect::MySql)
            .err()
            .unwrap_or_else(|| panic!("parsed: {sql}"))
    };
    assert!(matches!(
        e("SELECT 1 {{#if a}} x"),
        TemplateError::UnclosedBlock(_)
    ));
    assert!(matches!(
        e("SELECT 1 {{/if}}"),
        TemplateError::UnopenedBlock { .. }
    ));
    assert!(matches!(
        e("SELECT 1 {{else}}"),
        TemplateError::UnopenedBlock { .. }
    ));
    assert!(matches!(
        e("SELECT 1 {{#if a}} x {{else}} y {{else}} z {{/if}}"),
        TemplateError::DuplicateElse(_)
    ));
    assert!(matches!(
        e("SELECT 1 {{#if}} x {{/if}}"),
        TemplateError::EmptyCondition(_)
    ));
    assert!(matches!(
        e("SELECT 1 {{#if a +}} x {{/if}}"),
        TemplateError::InvalidExpression { .. }
    ));
    // a block marker inside a string literal is still a placeholder-in-a-string error
    assert!(matches!(
        e("SELECT '{{#if a}}' {{/if}}"),
        TemplateError::InsideString { .. }
    ));
    // identifier positions are still refused through a block
    assert!(matches!(
        e("SELECT 1 FROM {{#if a}} {{ t }} {{/if}}"),
        TemplateError::IdentifierPosition { .. }
    ));
    // a literal ? cannot be mixed with blocks either
    assert!(matches!(
        e("SELECT 1 WHERE a = ? {{#if b}} AND 1 {{/if}}"),
        TemplateError::MixedMarkers { .. }
    ));
    // `#iffy` is not a block marker: it stays an ordinary (zen) placeholder
    let t = parse("SELECT {{#iffy}}", Dialect::MySql);
    assert!(!t.is_dynamic() && t.placeholders().len() == 1);
}

#[test]
fn a_failing_condition_names_the_condition() {
    let t = parse(
        "SELECT 1 {{#if date('nope') > 1}} x {{/if}}",
        Dialect::MySql,
    );
    let e = run(&t, json!({})).unwrap_err().to_string();
    assert!(
        e.contains("#if date('nope') > 1") && e.contains("condition failed"),
        "{e}"
    );
}
