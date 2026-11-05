//! Placeholder parser + binder (no database needed).

use ms_zen_database::{
    bind, BindError, BindValue, Dialect, NodeLabel, QueryTemplate, TemplateError, MAX_BIND_PARAMS,
};
use rust_decimal::Decimal;
use serde_json::json;
use std::str::FromStr;
use std::sync::Arc;
use zen_expression::Variable;

const NODE: NodeLabel<'static> = NodeLabel {
    name: "rows",
    id: "db1",
};

fn mysql(sql: &str) -> QueryTemplate {
    QueryTemplate::parse(sql, Dialect::MySql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn pg(sql: &str) -> QueryTemplate {
    QueryTemplate::parse(sql, Dialect::Postgres).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(sql: &str, dialect: Dialect) -> TemplateError {
    match QueryTemplate::parse(sql, dialect) {
        Ok(_) => panic!("expected error for {sql}"),
        Err(e) => e,
    }
}

fn bind_json(
    t: &QueryTemplate,
    input: serde_json::Value,
) -> Result<(String, Vec<BindValue>), BindError> {
    let v = Variable::from(input);
    bind(t, &v, NODE).map(|b| (b.sql.to_string(), b.params))
}

fn text(s: &str) -> BindValue {
    BindValue::Text(s.to_string())
}

#[test]
fn scalar_placeholder_becomes_question_mark() {
    let t = mysql("SELECT * FROM t WHERE id = {{ id }}");
    assert_eq!(t.scalar_sql().as_ref(), "SELECT * FROM t WHERE id = ?");
    assert_eq!(t.placeholders().len(), 1);
    assert_eq!(t.placeholders()[0].source.as_ref(), "id");
}

#[test]
fn placeholders_without_spaces_and_with_defaults() {
    let t = mysql(
        "select 1 where 'us' = {{site}} and c = {{customer_id ?? 0}} and s = {{site ?? ''}} \
         and u > {{after_update_time ?? '1970-01-01 00:00:00'}} and id > {{after_id}}",
    );
    let sources: Vec<&str> = t.placeholders().iter().map(|p| p.source.as_ref()).collect();
    assert_eq!(
        sources,
        [
            "site",
            "customer_id ?? 0",
            "site ?? ''",
            "after_update_time ?? '1970-01-01 00:00:00'",
            "after_id"
        ]
    );
    assert_eq!(t.scalar_sql().matches('?').count(), 5);
}

#[test]
fn postgres_uses_numbered_markers() {
    let t = pg("SELECT * FROM t WHERE a = {{a}} AND b = {{b}}");
    assert_eq!(
        t.scalar_sql().as_ref(),
        "SELECT * FROM t WHERE a = $1 AND b = $2"
    );
}

#[test]
fn query_without_placeholders_is_unchanged() {
    let sql = "SELECT 'a?b', `c` FROM t";
    assert_eq!(mysql(sql).scalar_sql().as_ref(), sql);
}

#[test]
fn unterminated_placeholder_reports_position() {
    let e = err("SELECT 1\nWHERE a = {{ a", Dialect::MySql);
    assert!(
        matches!(e, TemplateError::Unterminated(p) if p.line == 2 && p.column == 11),
        "{e}"
    );
    assert!(e.to_string().contains("`{{` has no closing `}}`"), "{e}");
}

#[test]
fn empty_placeholder_is_rejected() {
    assert!(matches!(
        err("SELECT {{ }}", Dialect::MySql),
        TemplateError::Empty(_)
    ));
    assert!(matches!(
        err("SELECT {{}}", Dialect::MySql),
        TemplateError::Empty(_)
    ));
    assert!(err("SELECT {{ }}", Dialect::MySql)
        .to_string()
        .starts_with("empty placeholder `{{ }}` at"));
}

#[test]
fn nested_placeholder_is_rejected() {
    let e = err("SELECT {{ a {{ b }} }}", Dialect::MySql);
    assert!(matches!(e, TemplateError::Nested(_)), "{e}");
}

#[test]
fn placeholder_inside_single_quotes_is_rejected_with_like_hint() {
    let e = err(
        "SELECT * FROM t WHERE name LIKE '%{{term}}%'",
        Dialect::MySql,
    );
    assert!(matches!(e, TemplateError::InsideString { .. }), "{e}");
    let msg = e.to_string();
    assert!(
        msg.contains("a placeholder already stands for a complete value"),
        "{msg}"
    );
    assert!(msg.contains("LIKE {{ '%' + term + '%' }}"), "{msg}");
}

#[test]
fn placeholder_inside_double_quoted_mysql_string_is_rejected() {
    let e = err("SELECT \"{{x}}\"", Dialect::MySql);
    assert!(
        matches!(
            e,
            TemplateError::InsideString {
                kind: "double-quoted string",
                ..
            }
        ),
        "{e}"
    );
}

#[test]
fn placeholder_inside_quoted_identifier_is_rejected() {
    let e = err("SELECT `{{col}}` FROM t", Dialect::MySql);
    assert!(
        matches!(e, TemplateError::InsideIdentifier { quote: '`', .. }),
        "{e}"
    );
    assert!(e
        .to_string()
        .contains("placeholders bind values, they cannot name a table or column"));
    let e = err("SELECT \"{{col}}\" FROM t", Dialect::Postgres);
    assert!(
        matches!(e, TemplateError::InsideIdentifier { quote: '"', .. }),
        "{e}"
    );
}

#[test]
fn placeholder_in_identifier_position_is_rejected() {
    for (sql, kw) in [
        ("SELECT * FROM {{table}}", "FROM"),
        ("SELECT * FROM a JOIN {{b}} ON 1=1", "JOIN"),
        ("SELECT * FROM t ORDER BY {{col}}", "ORDER BY"),
        ("SELECT * FROM t group by {{col}}", "GROUP BY"),
        ("SELECT t.{{col}} FROM t", "."),
        ("SELECT a AS {{alias}} FROM t", "AS"),
    ] {
        match err(sql, Dialect::MySql) {
            TemplateError::IdentifierPosition { keyword, .. } => assert_eq!(keyword, kw, "{sql}"),
            other => panic!("{sql}: {other}"),
        }
    }
    let msg = err("SELECT * FROM t ORDER BY {{c}}", Dialect::MySql).to_string();
    assert!(
        msg.contains("MySQL accepts `ORDER BY ?` as a constant"),
        "{msg}"
    );
}

#[test]
fn from_as_value_separator_is_allowed() {
    mysql("SELECT EXTRACT(YEAR FROM {{d}})");
    mysql("SELECT SUBSTRING(name FROM {{n}}) FROM t");
    mysql("SELECT TRIM(LEADING 'x' FROM {{s}})");
    pg("SELECT * FROM t WHERE a IS DISTINCT FROM {{v}}");
    // a subquery's FROM is still an identifier position
    assert!(matches!(
        err(
            "SELECT * FROM t WHERE id IN (SELECT id FROM {{t2}})",
            Dialect::MySql
        ),
        TemplateError::IdentifierPosition { .. }
    ));
}

#[test]
fn literal_marker_mixed_with_placeholders_is_rejected() {
    let e = err("SELECT * FROM t WHERE a = ? AND b = {{b}}", Dialect::MySql);
    assert!(
        matches!(e, TemplateError::MixedMarkers { ref marker, .. } if marker == "?"),
        "{e}"
    );
    assert!(e
        .to_string()
        .contains("every bind slot must come from a placeholder"));
    let e = err(
        "SELECT * FROM t WHERE a = $1 AND b = {{b}}",
        Dialect::Postgres,
    );
    assert!(
        matches!(e, TemplateError::MixedMarkers { ref marker, .. } if marker == "$1"),
        "{e}"
    );
}

#[test]
fn markers_inside_strings_and_pg_operators_are_not_bind_slots() {
    mysql("SELECT '?' AS q, {{a}}");
    // Postgres `?` is the jsonb key-exists operator, not a marker.
    let t = pg("SELECT * FROM t WHERE data ? 'k' AND id = {{id}}");
    assert_eq!(
        t.scalar_sql().as_ref(),
        "SELECT * FROM t WHERE data ? 'k' AND id = $1"
    );
}

#[test]
fn comments_are_skipped() {
    let t = mysql("SELECT 1 -- {{ignored}} ?\n, {{a}} # {{also}}\n /* {{x}} ? */ , {{b}}");
    assert_eq!(t.placeholders().len(), 2);
    let t = pg("SELECT 1 /* outer /* {{nested}} */ still */, {{a}}");
    assert_eq!(t.placeholders().len(), 1);
    // MySQL needs whitespace after `--`; `1--{{a}}` is arithmetic.
    let t = mysql("SELECT 1--{{a}}");
    assert_eq!(t.placeholders().len(), 1);
}

#[test]
fn escaped_quotes_are_handled_per_dialect() {
    assert!(matches!(
        err("SELECT 'it''s {{x}}'", Dialect::MySql),
        TemplateError::InsideString { .. }
    ));
    mysql(r"SELECT 'it\'s' = {{x}}");
    // Postgres standard strings: backslash is literal, so the string ends at `\'`.
    pg(r"SELECT 'a\' = {{x}}");
    // E'' strings do honour backslash escapes.
    assert!(matches!(
        err(r"SELECT E'a\' {{x}}'", Dialect::Postgres),
        TemplateError::InsideString { .. }
    ));
}

#[test]
fn postgres_dollar_quotes() {
    assert!(matches!(
        err("SELECT $tag$ {{x}} $tag$", Dialect::Postgres),
        TemplateError::InsideString {
            kind: "dollar-quoted string",
            ..
        }
    ));
    let t = pg("SELECT $$text with ? and {$$, {{a}}");
    assert_eq!(t.scalar_sql().as_ref(), "SELECT $$text with ? and {$$, $1");
}

#[test]
fn closing_braces_inside_expression_strings() {
    let t = mysql("SELECT {{ x ?? '}}' }}");
    assert_eq!(t.placeholders()[0].source.as_ref(), "x ?? '}}'");
}

#[test]
fn invalid_expression_is_reported_at_parse_time() {
    let e = err("SELECT {{ 1 + }}", Dialect::MySql);
    assert!(matches!(e, TemplateError::InvalidExpression { .. }), "{e}");
}

#[test]
fn position_counts_characters_not_bytes() {
    let e = err("SELECT 'é', {{", Dialect::MySql);
    assert!(
        matches!(e, TemplateError::Unterminated(p) if p.column == 13),
        "{e}"
    );
}

#[test]
fn binds_scalars_by_type() {
    let t = mysql("SELECT {{s}}, {{i}}, {{d}}, {{b}}, {{neg}}, {{big}}");
    let (sql, params) = bind_json(
        &t,
        json!({"s": "us", "i": 42, "d": 10.5, "b": true, "neg": -3, "big": 1e20}),
    )
    .unwrap();
    assert_eq!(sql, "SELECT ?, ?, ?, ?, ?, ?");
    assert_eq!(
        params,
        vec![
            text("us"),
            BindValue::Int(42),
            BindValue::Decimal(Decimal::from_str("10.5").unwrap()),
            BindValue::Bool(true),
            BindValue::Int(-3),
            BindValue::Decimal(Decimal::from_str("100000000000000000000").unwrap()),
        ]
    );
}

#[test]
fn scalar_only_binding_reuses_prerendered_sql() {
    let t = mysql("SELECT {{a}}");
    let b = bind(&t, &Variable::from(json!({"a": 1})), NODE).unwrap();
    assert!(Arc::ptr_eq(&b.sql, t.scalar_sql()));
}

#[test]
fn null_coalescing_defaults() {
    let t = mysql("SELECT {{site ?? ''}}, {{customer_id ?? 0}}, {{after_update_time ?? '1970-01-01 00:00:00'}}");
    let (_, params) = bind_json(&t, json!({})).unwrap();
    assert_eq!(
        params,
        vec![text(""), BindValue::Int(0), text("1970-01-01 00:00:00")]
    );
    let (_, params) = bind_json(&t, json!({"site": "eu", "customer_id": 7})).unwrap();
    assert_eq!(params[0], text("eu"));
    assert_eq!(params[1], BindValue::Int(7));
}

#[test]
fn null_is_an_error_with_key_hint() {
    let t = mysql("SELECT {{customer.id}}");
    let e = bind_json(
        &t,
        json!({"customer": {"name": "x", "email": "y"}, "site": "us"}),
    )
    .unwrap_err();
    let msg = e.to_string();
    assert!(msg.starts_with("Database query placeholder \"{{ customer.id }}\" in node \"rows\" (db1) resolved to null."), "{msg}");
    assert!(
        msg.contains("write `?? <default>` in the placeholder if null is intended"),
        "{msg}"
    );
    assert!(
        msg.contains("\"customer\" exists and has keys: email, name"),
        "{msg}"
    );

    let t = mysql("SELECT {{customr_id}}");
    let msg = bind_json(&t, json!({"customer_id": 1}))
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("the node input exists and has keys: customer_id"),
        "{msg}"
    );
}

#[test]
fn explicit_null_value_is_an_error() {
    let t = mysql("SELECT {{a}}");
    let msg = bind_json(&t, json!({"a": null})).unwrap_err().to_string();
    assert!(msg.contains("resolved to null"), "{msg}");
    assert!(msg.contains("(\"a\" is present but null)"), "{msg}");
}

#[test]
fn list_expands_to_one_marker_per_element() {
    let t =
        mysql("SELECT * FROM p WHERE site = {{site}} AND sku IN ({{skus}}) AND id > {{after_id}}");
    let (sql, params) = bind_json(
        &t,
        json!({"site": "us", "skus": ["A", "B", "C"], "after_id": 5}),
    )
    .unwrap();
    assert_eq!(
        sql,
        "SELECT * FROM p WHERE site = ? AND sku IN (?, ?, ?) AND id > ?"
    );
    assert_eq!(
        params,
        vec![
            text("us"),
            text("A"),
            text("B"),
            text("C"),
            BindValue::Int(5)
        ]
    );
}

#[test]
fn postgres_list_numbering_continues() {
    let t = pg("SELECT * FROM p WHERE sku = ANY(ARRAY[{{skus}}]) AND id > {{id}}");
    let (sql, params) = bind_json(&t, json!({"skus": [1, 2], "id": 9})).unwrap();
    assert_eq!(
        sql,
        "SELECT * FROM p WHERE sku = ANY(ARRAY[$1, $2]) AND id > $3"
    );
    assert_eq!(params.len(), 3);
}

#[test]
fn single_element_list() {
    let t = mysql("SELECT {{ids}}");
    let (sql, params) = bind_json(&t, json!({"ids": [1]})).unwrap();
    assert_eq!(sql, "SELECT ?");
    assert_eq!(params, vec![BindValue::Int(1)]);
}

#[test]
fn empty_list_is_an_error_with_guard_hint() {
    let t = mysql("SELECT * FROM t WHERE id IN ({{ids}})");
    let msg = bind_json(&t, json!({"ids": []})).unwrap_err().to_string();
    assert!(
        msg.contains("resolved to an empty array, which has no SQL form"),
        "{msg}"
    );
    assert!(msg.contains("{{ len(ids) == 0 ? [-1] : ids }}"), "{msg}");
}

#[test]
fn empty_list_guard_expression_works() {
    let t = mysql("SELECT * FROM t WHERE id IN ({{ len(ids) == 0 ? [-1] : ids }})");
    let (sql, params) = bind_json(&t, json!({"ids": []})).unwrap();
    assert_eq!(sql, "SELECT * FROM t WHERE id IN (?)");
    assert_eq!(params, vec![BindValue::Int(-1)]);
}

#[test]
fn bad_list_elements_are_errors() {
    let t = mysql("SELECT {{v}}");
    let msg = bind_json(&t, json!({"v": [1, null]}))
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("resolved to an array whose element 1 is null"),
        "{msg}"
    );
    let msg = bind_json(&t, json!({"v": [[1, 2]]}))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("nested arrays are not expanded"), "{msg}");
    let msg = bind_json(&t, json!({"v": [{"a": 1}]}))
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("element 0 is a object, which has no bindable SQL form"),
        "{msg}"
    );
}

#[test]
fn object_value_is_an_error() {
    let t = mysql("SELECT {{v}}");
    let msg = bind_json(&t, json!({"v": {"a": 1}}))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("resolved to a object, which has no bindable SQL form; convert it in the expression first"), "{msg}");
}

#[test]
fn date_value_is_an_error_with_format_hint() {
    let t = mysql("SELECT {{ d('2024-01-02') }}");
    let msg = bind_json(&t, json!({})).unwrap_err().to_string();
    assert!(msg.contains("which has no bindable SQL form"), "{msg}");
    assert!(msg.contains(".format('%Y-%m-%d %H:%M:%S')"), "{msg}");
    // the suggested conversion binds as text
    let t = mysql("SELECT {{ d('2024-01-02').format('%Y-%m-%d %H:%M:%S') }}");
    let (_, params) = bind_json(&t, json!({})).unwrap();
    assert_eq!(params, vec![text("2024-01-02 00:00:00")]);
}

#[test]
fn expressions_are_evaluated_by_zen() {
    let t =
        mysql("SELECT {{ '%' + term + '%' }}, {{ qty * 2 }}, {{ upper(site) }}, {{ len(skus) }}");
    let (_, params) = bind_json(
        &t,
        json!({"term": "ab", "qty": 3, "site": "us", "skus": [1, 2]}),
    )
    .unwrap();
    assert_eq!(
        params,
        vec![
            text("%ab%"),
            BindValue::Int(6),
            text("US"),
            BindValue::Int(2)
        ]
    );
}

#[test]
fn nodes_context_is_available() {
    let t = mysql("SELECT {{ $nodes.request.id }}");
    let (_, params) = bind_json(&t, json!({"$nodes": {"request": {"id": 11}}})).unwrap();
    assert_eq!(params, vec![BindValue::Int(11)]);
}

#[test]
fn injection_attempts_stay_inside_parameters() {
    let t = mysql("SELECT * FROM users WHERE name = {{name}} AND id IN ({{ids}})");
    for evil in [
        "' OR '1'='1",
        "1; DROP TABLE users; --",
        "\\'; SELECT SLEEP(10); #",
        "{{ 1 }}",
        "?",
        "x\" OR \"1\"=\"1",
    ] {
        let (sql, params) =
            bind_json(&t, json!({"name": evil, "ids": [evil, "2) OR (1=1"]})).unwrap();
        assert_eq!(sql, "SELECT * FROM users WHERE name = ? AND id IN (?, ?)");
        assert_eq!(params[0], text(evil));
        assert_eq!(params[1], text(evil));
        assert_eq!(params[2], text("2) OR (1=1"));
    }
}

#[test]
fn too_many_bind_parameters() {
    let t = mysql("SELECT * FROM t WHERE id IN ({{ids}})");
    let ids: Vec<u32> = (0..=MAX_BIND_PARAMS as u32).collect();
    match bind_json(&t, json!({ "ids": ids })) {
        Err(BindError::TooManyParams { count, .. }) => assert_eq!(count, MAX_BIND_PARAMS + 1),
        other => panic!("{other:?}"),
    }
    let ids: Vec<u32> = (0..MAX_BIND_PARAMS as u32).collect();
    assert!(bind_json(&t, json!({ "ids": ids })).is_ok());
}

#[test]
fn evaluation_errors_are_reported() {
    let t = mysql("SELECT {{ d('not a date') }}");
    let msg = bind_json(&t, json!({}))
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(msg.contains("Database query placeholder"), "{msg}");
}

/// The studio's fixed values (a constant in the picture: "status is \"active\"") are zen literals inside the
/// placeholder. They bind as parameters like any value: the SQL text holds only markers, never the literal.
#[test]
fn literal_placeholders_bind_as_parameters_never_as_sql_text() {
    let sql = "SELECT sku FROM products WHERE status = {{ 'active' }} AND cost = {{ 4.20 }} AND qty > {{ 42 }} AND live = {{ true }} AND note = {{ \"it's\" }}";
    for (t, markers) in [
        (mysql(sql), ["?", "?", "?", "?", "?"]),
        (pg(sql), ["$1", "$2", "$3", "$4", "$5"]),
    ] {
        let (text_sql, params) = bind_json(&t, json!({})).unwrap();
        for m in markers {
            assert!(text_sql.contains(m), "{text_sql}");
        }
        for literal in ["active", "4.20", "4.2", "42", "true", "it's"] {
            assert!(
                !text_sql.contains(literal),
                "literal {literal:?} reached the SQL text: {text_sql}"
            );
        }
        assert_eq!(
            params,
            vec![
                text("active"),
                BindValue::Decimal(Decimal::from_str("4.2").unwrap()),
                BindValue::Int(42),
                BindValue::Bool(true),
                text("it's"),
            ]
        );
    }
}

#[test]
fn templates_know_whether_they_only_read() {
    let parse = |sql: &str| QueryTemplate::parse(sql, Dialect::MySql).unwrap();
    assert!(parse("SELECT a FROM t WHERE b = {{ b }}").is_read_only());
    assert!(parse("WITH x AS (SELECT {{ b }} AS b) SELECT b FROM x").is_read_only());
    assert!(!parse("INSERT INTO t (b) VALUES ({{ b }})").is_read_only());
    assert!(!parse("UPDATE t SET b = {{ b }}").is_read_only());
    assert!(!parse("SELECT a FROM t WHERE b = {{ b }} FOR UPDATE").is_read_only());
}
