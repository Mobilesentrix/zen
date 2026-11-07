//! The SQL of a Lookup step: its custom query, or the SELECT the builder's tables and
//! conditions describe.

use super::literal::{quote_ident, quote_sql_string};
use super::where_clause::{build_where_group, FieldQualifier, WhereOptions};
use serde_json::{Map, Value};

/// A (tableName, columns) pair of `selectedTables`.
struct SelectedTable<'a> {
    name: &'a str,
    columns: Vec<&'a str>,
}

/// The step's SQL; `Ok(None)` when the content holds no query source yet.
pub(crate) fn compile_query(
    content: &Map<String, Value>,
    database: &str,
) -> Result<Option<String>, String> {
    if content.get("useCustomQuery").and_then(Value::as_bool) == Some(true) {
        let custom = content
            .get("customQuery")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Ok((!custom.is_empty()).then(|| custom.to_string()));
    }

    let Some(raw) = content.get("selectedTables").and_then(Value::as_array) else {
        return Ok(None);
    };
    let tables = parse_selected_tables(raw);
    if tables.is_empty() {
        return Ok(None);
    }
    let where_group = content.get("whereClauseGroup").and_then(Value::as_object);

    if let [table] = tables.as_slice() {
        single_table_query(table, database, where_group).map(Some)
    } else {
        multi_table_query(&tables, database, where_group).map(Some)
    }
}

/// A plain SELECT: the result is a flat rowset, the same shape as a custom query's.
fn single_table_query(
    table: &SelectedTable<'_>,
    database: &str,
    where_group: Option<&Map<String, Value>>,
) -> Result<String, String> {
    let columns: Vec<String> = table.columns.iter().map(|c| quote_ident(c)).collect();
    let from = with_where_clause(
        qualified_table(database, table.name),
        table.name,
        |_, column| quote_ident(column),
        where_group,
    )?;
    Ok(format!("SELECT {} FROM {from}", columns.join(", ")))
}

/// One JSON array per table through `to_json(named_struct(… array_agg(…)))`. Tables are queried
/// independently (no joins); each keeps only its own conditions.
fn multi_table_query(
    tables: &[SelectedTable<'_>],
    database: &str,
    where_group: Option<&Map<String, Value>>,
) -> Result<String, String> {
    let mut parts = Vec::with_capacity(tables.len());
    for table in tables {
        let pairs: Vec<String> = table
            .columns
            .iter()
            .map(|column| format!("{}, t.{}", quote_sql_string(column), quote_ident(column)))
            .collect();
        let from = with_where_clause(
            format!("{} t", qualified_table(database, table.name)),
            table.name,
            |_, column| format!("t.{}", quote_ident(column)),
            where_group,
        )?;
        parts.push(format!(
            "{}, (SELECT array_agg(named_struct({})) FROM {from})",
            quote_sql_string(table.name),
            pairs.join(", ")
        ));
    }
    Ok(format!(
        "SELECT to_json(named_struct({})) AS result",
        parts.join(", ")
    ))
}

/// `` `db`.`table` ``, or `` `table` `` without a database.
fn qualified_table(database: &str, table: &str) -> String {
    if database.is_empty() {
        quote_ident(table)
    } else {
        format!("{}.{}", quote_ident(database), quote_ident(table))
    }
}

/// `from` followed by the WHERE clause of the group, kept to `table`'s own conditions.
fn with_where_clause(
    from: String,
    table: &str,
    qualify: FieldQualifier,
    where_group: Option<&Map<String, Value>>,
) -> Result<String, String> {
    let Some(group) = where_group else {
        return Ok(from);
    };
    let options = WhereOptions {
        qualify,
        table_filter: table,
    };
    Ok(match build_where_group(group, &options)? {
        Some(clause) => format!("{from} WHERE {clause}"),
        None => from,
    })
}

/// Valid (non-empty name, non-empty columns) entries; malformed or empty ones are skipped.
fn parse_selected_tables(raw: &[Value]) -> Vec<SelectedTable<'_>> {
    raw.iter()
        .filter_map(Value::as_object)
        .filter_map(|fields| {
            let name = fields
                .get("tableName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())?;
            let columns: Vec<&str> = fields
                .get("columns")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .filter(|column| !column.is_empty())
                .collect();
            (!columns.is_empty()).then_some(SelectedTable { name, columns })
        })
        .collect()
}
