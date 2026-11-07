//! The builder's `whereClauseGroup` tree rendered into a StarRocks WHERE-clause body.
//!
//! Only operators from a closed set reach the SQL text; values stored in the rule are quoted
//! literals; values taken from the request are engine placeholders (`{{ field }}`) that the
//! engine binds as parameters, never SQL text.

use super::literal::{format_typed, go_quote, quote_ident};
use serde_json::{Map, Value};

/// Renders a (table, column) pair into a SQL field reference.
pub(crate) type FieldQualifier = fn(&str, &str) -> String;

pub(crate) struct WhereOptions<'a> {
    pub(crate) qualify: FieldQualifier,
    /// When non-empty, only conditions on this table are kept.
    pub(crate) table_filter: &'a str,
}

/// The operators the editor offers. The operator text of the rule itself never reaches the SQL.
const WHERE_OPERATORS: &[&str] = &[
    "=",
    "!=",
    "<>",
    ">",
    ">=",
    "<",
    "<=",
    "LIKE",
    "NOT LIKE",
    "ILIKE",
    "NOT ILIKE",
    "REGEXP",
    "IN",
    "NOT IN",
    "BETWEEN",
    "NOT BETWEEN",
    "IS NULL",
    "IS NOT NULL",
    "TRUE",
    "FALSE",
];

/// A rendered condition. `guard` is set when it applies only when the request gives its
/// field(s): it is then emitted inside an engine `{{#if guard}} … {{/if}}` block.
struct Part {
    sql: String,
    guard: Option<String>,
}

/// Renders a `whereClauseGroup` tree into a WHERE-clause body (without `WHERE `), qualifying
/// fields as `table`.`column`. `Ok(None)` when no usable condition remains after dropping
/// incomplete ones; an error when a condition cannot be rendered safely.
pub fn build_where_clause(group: &Map<String, Value>) -> Result<Option<String>, String> {
    build_where_group(
        group,
        &WhereOptions {
            qualify: |table, column| format!("{}.{}", quote_ident(table), quote_ident(column)),
            table_filter: "",
        },
    )
}

/// A group joins its emitted children with its `logicalJoin`, in parentheses only when two or
/// more are emitted.
pub(crate) fn build_where_group(
    group: &Map<String, Value>,
    options: &WhereOptions<'_>,
) -> Result<Option<String>, String> {
    let Some(children) = group.get("children").and_then(Value::as_array) else {
        return Ok(None);
    };
    let join = group_join(group);

    let mut parts = Vec::new();
    let mut optional = Vec::new();
    for child in children.iter().filter_map(Value::as_object) {
        match build_where_child(child, options)? {
            None => {}
            Some(Part {
                sql,
                guard: Some(guard),
            }) => optional.push((guard, sql)),
            Some(Part { sql, guard: None }) => parts.push(sql),
        }
    }

    if !optional.is_empty() {
        return join_optional_parts(join, &parts, &optional).map(Some);
    }
    Ok(match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(format!("({})", parts.join(&format!(" {join} ")))),
    })
}

/// The group's `logicalJoin`: AND unless it says OR.
fn group_join(group: &Map<String, Value>) -> &'static str {
    match group
        .get("logicalJoin")
        .and_then(Value::as_str)
        .map(|join| join.trim().to_uppercase())
        .as_deref()
    {
        Some("OR") => "OR",
        _ => "AND",
    }
}

/// A child without a known `type` is a group when it has `children`, a condition otherwise.
fn build_where_child(
    child: &Map<String, Value>,
    options: &WhereOptions<'_>,
) -> Result<Option<Part>, String> {
    let child_type = child.get("type").and_then(Value::as_str).unwrap_or("");
    let has_children = child.contains_key("children");
    if child_type == "condition" || (child_type != "group" && !has_children) {
        return build_where_condition(child, options);
    }
    Ok(build_where_group(child, options)?.map(|sql| Part { sql, guard: None }))
}

/// An AND group holding optional conditions:
/// `(fixed AND fixed {{#if f}} AND cond {{/if}} …)`. Skipping an optional condition leaves the
/// rest intact; with no fixed condition the group starts from `1 = 1`. An OR group cannot hold
/// one (skipping a term of an OR changes what the group means), so the rule is refused.
fn join_optional_parts(
    join: &str,
    parts: &[String],
    optional: &[(String, String)],
) -> Result<String, String> {
    if join != "AND" {
        return Err(format!(
            "a condition that applies only when the request gives {} must be joined with AND, not {join}",
            optional[0].0
        ));
    }
    let mut sql = if parts.is_empty() {
        "1 = 1".to_string()
    } else {
        parts.join(" AND ")
    };
    for (guard, condition) in optional {
        sql.push_str(&format!(" {{{{#if {guard}}}}} AND {condition} {{{{/if}}}}"));
    }
    Ok(format!("({sql})"))
}

/// What a request field may name: a path such as `colour`, `filters.colour` or `items[0].sku`
/// (ASCII letters, digits, underscores, dots and `[index]`), so it can neither close the
/// placeholder nor add an operator.
fn is_request_field(field: &str) -> bool {
    let bytes = field.as_bytes();
    let mut at = 0;
    let identifier = |at: &mut usize| -> bool {
        if *at < bytes.len() && (bytes[*at].is_ascii_alphabetic() || bytes[*at] == b'_') {
            *at += 1;
            while *at < bytes.len() && (bytes[*at].is_ascii_alphanumeric() || bytes[*at] == b'_') {
                *at += 1;
            }
            true
        } else {
            false
        }
    };
    if !identifier(&mut at) {
        return false;
    }
    while at < bytes.len() {
        match bytes[at] {
            b'.' => {
                at += 1;
                if !identifier(&mut at) {
                    return false;
                }
            }
            b'[' => {
                at += 1;
                let start = at;
                while at < bytes.len() && bytes[at].is_ascii_digit() {
                    at += 1;
                }
                if at == start || at >= bytes.len() || bytes[at] != b']' {
                    return false;
                }
                at += 1;
            }
            _ => return false,
        }
    }
    true
}

fn request_field(
    condition: &Map<String, Value>,
    key: &str,
    table: &str,
    column: &str,
) -> Result<String, String> {
    let raw = condition.get(key).and_then(Value::as_str).unwrap_or("");
    let field = raw.trim();
    if !is_request_field(field) {
        return Err(format!(
            "{table}.{column}: {key} {} is not a request field name",
            go_quote(raw)
        ));
    }
    Ok(field.to_string())
}

/// A condition whose value comes from the request (`valueSource: "request"`). IN / NOT IN take a
/// list field (the engine expands it); BETWEEN takes `requestField` and `secondRequestField`.
/// Optional by default (applies only when the request gives the field); `optional: false`
/// always applies it.
fn build_request_condition(
    condition: &Map<String, Value>,
    op: &str,
    field: &str,
    table: &str,
    column: &str,
) -> Result<Option<Part>, String> {
    let first = request_field(condition, "requestField", table, column)?;
    let placeholder = format!("{{{{ {first} }}}}");
    let mut guard = first.clone();

    let sql = match op {
        "IN" | "NOT IN" => format!("{field} {op} ({placeholder})"),
        "BETWEEN" | "NOT BETWEEN" => {
            let second = request_field(condition, "secondRequestField", table, column)?;
            guard = format!("{first} and {second}");
            format!("{field} {op} {placeholder} AND {{{{ {second} }}}}")
        }
        "ILIKE" | "NOT ILIKE" => case_insensitive_like(field, op, &placeholder),
        _ => format!("{field} {op} {placeholder}"),
    };

    let always = condition.get("optional").and_then(Value::as_bool) == Some(false);
    Ok(Some(Part {
        sql,
        guard: (!always).then_some(guard),
    }))
}

/// ILIKE emulated with LOWER(), which StarRocks has (ILIKE it does not).
fn case_insensitive_like(field: &str, op: &str, value: &str) -> String {
    let like = if op == "NOT ILIKE" {
        "NOT LIKE"
    } else {
        "LIKE"
    };
    format!("LOWER({field}) {like} LOWER({value})")
}

fn operator_takes_value(op: &str) -> bool {
    !matches!(op, "IS NULL" | "IS NOT NULL" | "TRUE" | "FALSE")
}

/// The column a condition compares and its declared type.
struct ConditionColumn<'a> {
    table: &'a str,
    column: &'a str,
    value_type: &'a str,
}

impl ConditionColumn<'_> {
    fn format(&self, value: Option<&Value>) -> Result<Option<String>, String> {
        format_typed(value, self.value_type)
            .map_err(|e| format!("{}.{}: {e}", self.table, self.column))
    }
}

/// One condition. `Ok(None)` drops it: missing table/column/operator, a required value absent
/// or empty, an empty IN list, a half-filled BETWEEN, or a condition on another table than the
/// active filter. An operator outside [`WHERE_OPERATORS`] or a value that cannot be rendered for
/// its declared type is an error.
fn build_where_condition(
    condition: &Map<String, Value>,
    options: &WhereOptions<'_>,
) -> Result<Option<Part>, String> {
    let text = |key: &str| {
        condition
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
    };
    let (table, column, operator) = (text("table"), text("column"), text("operator"));
    if table.is_empty() || column.is_empty() || operator.is_empty() {
        return Ok(None);
    }
    if !options.table_filter.is_empty() && table != options.table_filter {
        return Ok(None);
    }

    // Case and inner whitespace are normalised ("not   like" -> "NOT LIKE") before the check.
    let op = operator
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    if !WHERE_OPERATORS.contains(&op.as_str()) {
        return Err(format!(
            "unsupported where operator {} for {table}.{column}",
            go_quote(operator)
        ));
    }

    let field = (options.qualify)(table, column);

    let source = condition
        .get("valueSource")
        .and_then(Value::as_str)
        .unwrap_or("");
    if source.trim().eq_ignore_ascii_case("request") && operator_takes_value(&op) {
        return build_request_condition(condition, &op, &field, table, column);
    }

    let column = ConditionColumn {
        table,
        column,
        value_type: condition
            .get("valueType")
            .and_then(Value::as_str)
            .unwrap_or(""),
    };
    Ok(build_literal_condition(condition, &op, &field, &column)?
        .map(|sql| Part { sql, guard: None }))
}

/// A condition whose value is stored in the rule.
fn build_literal_condition(
    condition: &Map<String, Value>,
    op: &str,
    field: &str,
    column: &ConditionColumn<'_>,
) -> Result<Option<String>, String> {
    match op {
        "IS NULL" => return Ok(Some(format!("{field} IS NULL"))),
        "IS NOT NULL" => return Ok(Some(format!("{field} IS NOT NULL"))),
        "TRUE" => return Ok(Some(format!("{field} = TRUE"))),
        "FALSE" => return Ok(Some(format!("{field} = FALSE"))),
        "IN" | "NOT IN" => return build_in_condition(condition, op, field, column),
        "BETWEEN" | "NOT BETWEEN" => return build_between_condition(condition, op, field, column),
        _ => {}
    }

    let Some(value) = column.format(condition.get("value"))? else {
        return Ok(None);
    };
    if op == "ILIKE" || op == "NOT ILIKE" {
        return Ok(Some(case_insensitive_like(field, op, &value)));
    }
    Ok(Some(format!("{field} {op} {value}")))
}

fn build_in_condition(
    condition: &Map<String, Value>,
    op: &str,
    field: &str,
    column: &ConditionColumn<'_>,
) -> Result<Option<String>, String> {
    let Some(values) = condition.get("value").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut literals = Vec::with_capacity(values.len());
    for value in values {
        if let Some(literal) = column.format(Some(value))? {
            literals.push(literal);
        }
    }
    if literals.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!("{field} {op} ({})", literals.join(", "))))
}

fn build_between_condition(
    condition: &Map<String, Value>,
    op: &str,
    field: &str,
    column: &ConditionColumn<'_>,
) -> Result<Option<String>, String> {
    let low = column.format(condition.get("value"))?;
    let high = column.format(condition.get("secondValue"))?;
    match (low, high) {
        (Some(low), Some(high)) => Ok(Some(format!("{field} {op} {low} AND {high}"))),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group(join: &str, conditions: Value) -> Map<String, Value> {
        json!({"logicalJoin": join, "children": conditions})
            .as_object()
            .unwrap()
            .clone()
    }

    fn render(conditions: Value) -> Result<Option<String>, String> {
        build_where_clause(&group("AND", conditions))
    }

    /// Every operator the editor offers renders to fixed SQL; the operator itself is never
    /// copied from the rule into the query.
    #[test]
    fn renders_every_editor_operator() {
        let cases = [
            (
                json!({"table":"o","column":"a","operator":"=","value":"x"}),
                "`o`.`a` = 'x'",
            ),
            (
                json!({"table":"o","column":"a","operator":"!=","value":5}),
                "`o`.`a` != 5",
            ),
            (
                json!({"table":"o","column":"a","operator":"<>","value":"5","valueType":"string"}),
                "`o`.`a` <> '5'",
            ),
            (
                json!({"table":"o","column":"a","operator":">","value":"5","valueType":"number"}),
                "`o`.`a` > 5",
            ),
            (
                json!({"table":"o","column":"a","operator":">=","value":-1.5,"valueType":"number"}),
                "`o`.`a` >= -1.5",
            ),
            (
                json!({"table":"o","column":"a","operator":"<","value":"2024-01-01","valueType":"date"}),
                "`o`.`a` < '2024-01-01'",
            ),
            (
                json!({"table":"o","column":"a","operator":"<=","value":true,"valueType":"boolean"}),
                "`o`.`a` <= TRUE",
            ),
            (
                json!({"table":"o","column":"a","operator":"LIKE","value":"x%"}),
                "`o`.`a` LIKE 'x%'",
            ),
            (
                json!({"table":"o","column":"a","operator":" not   like ","value":"x"}),
                "`o`.`a` NOT LIKE 'x'",
            ),
            (
                json!({"table":"o","column":"a","operator":"ILIKE","value":"x"}),
                "LOWER(`o`.`a`) LIKE LOWER('x')",
            ),
            (
                json!({"table":"o","column":"a","operator":"NOT ILIKE","value":"x"}),
                "LOWER(`o`.`a`) NOT LIKE LOWER('x')",
            ),
            (
                json!({"table":"o","column":"a","operator":"REGEXP","value":"^a"}),
                "`o`.`a` REGEXP '^a'",
            ),
            (
                json!({"table":"o","column":"a","operator":"IN","value":["a",1]}),
                "`o`.`a` IN ('a', 1)",
            ),
            (
                json!({"table":"o","column":"a","operator":"NOT IN","value":["1","2"],"valueType":"number"}),
                "`o`.`a` NOT IN (1, 2)",
            ),
            (
                json!({"table":"o","column":"a","operator":"BETWEEN","value":1,"secondValue":2}),
                "`o`.`a` BETWEEN 1 AND 2",
            ),
            (
                json!({"table":"o","column":"a","operator":"NOT BETWEEN","value":"2024-01-01","secondValue":"2024-02-01","valueType":"date"}),
                "`o`.`a` NOT BETWEEN '2024-01-01' AND '2024-02-01'",
            ),
            (
                json!({"table":"o","column":"a","operator":"IS NULL"}),
                "`o`.`a` IS NULL",
            ),
            (
                json!({"table":"o","column":"a","operator":"is not null"}),
                "`o`.`a` IS NOT NULL",
            ),
            (
                json!({"table":"o","column":"a","operator":"TRUE"}),
                "`o`.`a` = TRUE",
            ),
            (
                json!({"table":"o","column":"a","operator":"FALSE"}),
                "`o`.`a` = FALSE",
            ),
        ];
        for (condition, want) in cases {
            assert_eq!(render(json!([condition])).unwrap().as_deref(), Some(want));
        }
    }

    /// Anything that would let rule content reach the query unquoted is an error, not a
    /// silently dropped or spliced condition.
    #[test]
    fn rejects_sql_injection_vectors() {
        let cases = [
            (
                json!({"table":"o","column":"a","operator":"= 1 OR 1=1 --","value":"x"}),
                "operator",
            ),
            (
                json!({"table":"o","column":"a","operator":"= (SELECT 1) OR a","value":"x"}),
                "operator",
            ),
            (
                json!({"table":"o","column":"a","operator":"=","value":"1 OR 1=1","valueType":"number"}),
                "not a number",
            ),
            (
                json!({"table":"o","column":"a","operator":"IN","value":["1","2) OR (1=1"],"valueType":"number"}),
                "not a number",
            ),
            (
                json!({"table":"o","column":"a","operator":"BETWEEN","value":"1","secondValue":"2 OR 1=1","valueType":"number"}),
                "not a number",
            ),
        ];
        for (condition, want) in cases {
            let error = render(json!([condition.clone()])).unwrap_err();
            assert!(error.contains(want), "{condition}: {error}");
        }
    }

    #[test]
    fn drops_incomplete_conditions() {
        let got = render(json!([
            {"table":"o","column":"a","operator":"=","value":""},
            {"table":"o","column":"","operator":"=","value":"x"},
            {"table":"o","column":"a","operator":"IN","value":[]},
            {"table":"o","column":"a","operator":"BETWEEN","value":1},
            {"table":"o","column":"a","operator":"=","value":"kept"}
        ]))
        .unwrap();
        assert_eq!(got.as_deref(), Some("`o`.`a` = 'kept'"));
    }

    /// A condition whose value comes from the request compiles to a bound placeholder; by
    /// default it applies only when the request gives the field (`{{#if}}` block).
    #[test]
    fn request_values_become_bound_placeholders() {
        let active = json!({"table":"p","column":"status","operator":"=","value":"active"});
        let cases = [
            (
                json!([{"table":"p","column":"colour","operator":"=","valueSource":"request","requestField":"colour"}]),
                "(1 = 1 {{#if colour}} AND `p`.`colour` = {{ colour }} {{/if}})",
            ),
            (
                json!([{"table":"p","column":"colour","operator":"=","valueSource":"request","requestField":"colour"}, active]),
                "(`p`.`status` = 'active' {{#if colour}} AND `p`.`colour` = {{ colour }} {{/if}})",
            ),
            (
                json!([active, {"table":"p","column":"colour","operator":"=","valueSource":"request","requestField":"colour"},
                       {"table":"p","column":"quality","operator":"!=","valueSource":"Request","requestField":" quality "}]),
                "(`p`.`status` = 'active' {{#if colour}} AND `p`.`colour` = {{ colour }} {{/if}} {{#if quality}} AND `p`.`quality` != {{ quality }} {{/if}})",
            ),
            (
                json!([active, {"table":"p","column":"colour","operator":"=","valueSource":"request","requestField":"colour","optional":false}]),
                "(`p`.`status` = 'active' AND `p`.`colour` = {{ colour }})",
            ),
            (
                json!([{"table":"p","column":"brand","operator":"IN","valueSource":"request","requestField":"brands"}]),
                "(1 = 1 {{#if brands}} AND `p`.`brand` IN ({{ brands }}) {{/if}})",
            ),
            (
                json!([{"table":"p","column":"brand","operator":"not in","valueSource":"request","requestField":"brands","optional":false}]),
                "`p`.`brand` NOT IN ({{ brands }})",
            ),
            (
                json!([{"table":"p","column":"price","operator":"BETWEEN","valueSource":"request","requestField":"minPrice","secondRequestField":"maxPrice"}]),
                "(1 = 1 {{#if minPrice and maxPrice}} AND `p`.`price` BETWEEN {{ minPrice }} AND {{ maxPrice }} {{/if}})",
            ),
            (
                json!([{"table":"p","column":"name","operator":"ILIKE","valueSource":"request","requestField":"q"}]),
                "(1 = 1 {{#if q}} AND LOWER(`p`.`name`) LIKE LOWER({{ q }}) {{/if}})",
            ),
            (
                json!([{"table":"p","column":"sku","operator":"=","valueSource":"request","requestField":"items[0].sku","optional":false}]),
                "`p`.`sku` = {{ items[0].sku }}",
            ),
            (
                json!([{"table":"p","column":"a","operator":"IS NULL","valueSource":"request","requestField":"x"}]),
                "`p`.`a` IS NULL",
            ),
            (
                json!([
                    {"type":"group","logicalJoin":"OR","children":[active, {"table":"p","column":"status","operator":"=","value":"new"}]},
                    {"type":"group","logicalJoin":"AND","children":[{"table":"p","column":"colour","operator":"=","valueSource":"request","requestField":"colour"}]}
                ]),
                "((`p`.`status` = 'active' OR `p`.`status` = 'new') AND (1 = 1 {{#if colour}} AND `p`.`colour` = {{ colour }} {{/if}}))",
            ),
        ];
        for (conditions, want) in cases {
            assert_eq!(render(conditions).unwrap().as_deref(), Some(want));
        }
    }

    /// A request field is a path only, and an optional condition cannot sit in an OR group.
    #[test]
    fn request_values_refused() {
        let cases = [
            (
                "AND",
                json!([{"table":"p","column":"c","operator":"=","valueSource":"request","requestField":"x }} OR 1=1 {{ y"}]),
                "not a request field name",
            ),
            (
                "AND",
                json!([{"table":"p","column":"c","operator":"=","valueSource":"request","requestField":"a ?? 'x'"}]),
                "not a request field name",
            ),
            (
                "AND",
                json!([{"table":"p","column":"c","operator":"=","valueSource":"request"}]),
                "not a request field name",
            ),
            (
                "AND",
                json!([{"table":"p","column":"c","operator":"=","valueSource":"request","requestField":"#if x"}]),
                "not a request field name",
            ),
            (
                "AND",
                json!([{"table":"p","column":"c","operator":"BETWEEN","valueSource":"request","requestField":"lo"}]),
                "secondRequestField",
            ),
            (
                "OR",
                json!([{"table":"p","column":"c","operator":"=","value":"a"},{"table":"p","column":"d","operator":"=","valueSource":"request","requestField":"d"}]),
                "joined with AND",
            ),
        ];
        for (join, conditions, want) in cases {
            let error = build_where_clause(&group(join, conditions.clone())).unwrap_err();
            assert!(error.contains(want), "{conditions}: {error}");
        }
    }

    #[test]
    fn required_request_value_in_or_group() {
        let got = build_where_clause(&group(
            "OR",
            json!([{"table":"p","column":"c","operator":"=","value":"a"},
                   {"table":"p","column":"d","operator":"=","valueSource":"request","requestField":"d","optional":false}]),
        ))
        .unwrap();
        assert_eq!(got.as_deref(), Some("(`p`.`c` = 'a' OR `p`.`d` = {{ d }})"));
    }

    #[test]
    fn request_field_names() {
        for ok in ["a", "_x", "filters.colour", "items[0].sku", "a[12][3].b_2"] {
            assert!(is_request_field(ok), "{ok}");
        }
        for bad in [
            "", "1a", "a.", "a[]", "a[x]", "a[1", "a b", "a-b", "a.[0]", "é",
        ] {
            assert!(!is_request_field(bad), "{bad}");
        }
    }
}
