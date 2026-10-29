//! Row -> JSON decoding for MySQL-protocol (incl. StarRocks) and PostgreSQL results.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{Map, Number, Value};
use sqlx::mysql::{MySqlRow, MySqlValueRef};
use sqlx::postgres::{PgRow, PgValueRef};
use sqlx::types::chrono;
use sqlx::{Column, Decode, MySql, Postgres, Row, TypeInfo, ValueRef};

pub(crate) type DecodeResult = Result<Value, String>;

/// Upper-cased type name of every column; computed once per result set (every row of a
/// result shares the column list).
pub(crate) fn column_types<R: Row>(row: &R) -> Vec<String> {
    row.columns()
        .iter()
        .map(|column| column.type_info().name().to_ascii_uppercase())
        .collect()
}

pub(crate) fn mysql_row(row: &MySqlRow, types: &[String]) -> DecodeResult {
    let mut object = Map::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(index).map_err(|e| e.to_string())?;
        let type_name = types.get(index).map(String::as_str).unwrap_or("");
        let mut value = mysql_value(raw, type_name)?;
        parse_json_strings(&mut value);
        object.insert(column.name().to_owned(), value);
    }
    Ok(Value::Object(object))
}

fn mysql_as<T>(value: MySqlValueRef<'_>) -> Result<T, String>
where
    T: for<'r> Decode<'r, MySql>,
{
    <T as Decode<MySql>>::decode(value).map_err(|e| e.to_string())
}

fn mysql_value(value: MySqlValueRef<'_>, type_name: &str) -> DecodeResult {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match type_name {
        "JSON" => mysql_as::<Value>(value),
        "BIT" => mysql_as::<u64>(value).map(Value::from),
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" => mysql_as::<u64>(value).map(Value::from),
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
            mysql_as::<i64>(value).map(Value::from)
        }
        "FLOAT" => mysql_as::<f32>(value).map(|v| float(v as f64)),
        "DOUBLE" => mysql_as::<f64>(value).map(float),
        "DECIMAL" | "NUMERIC" | "NEWDECIMAL" => mysql_as::<String>(value).map(decimal_string),
        "BOOL" | "BOOLEAN" => mysql_as::<bool>(value).map(Value::Bool),
        "DATE" => mysql_as::<chrono::NaiveDate>(value)
            .map(|d| Value::String(d.format("%Y-%m-%d").to_string())),
        "TIME" => mysql_as::<chrono::NaiveTime>(value)
            .map(|t| Value::String(t.format("%H:%M:%S").to_string())),
        "DATETIME" | "TIMESTAMP" => mysql_as::<chrono::NaiveDateTime>(value)
            .map(|dt| Value::String(dt.format("%Y-%m-%d %H:%M:%S").to_string())),
        "YEAR" => mysql_as::<u16>(value).map(Value::from),
        t if t.contains("BINARY")
            || t.contains("BLOB")
            || t.contains("GEOMETRY")
            || matches!(
                t,
                "POINT"
                    | "MULTIPOINT"
                    | "LINESTRING"
                    | "MULTILINESTRING"
                    | "POLYGON"
                    | "MULTIPOLYGON"
            ) =>
        {
            mysql_as::<Vec<u8>>(value).map(|b| Value::String(STANDARD.encode(b)))
        }
        _ => mysql_as::<String>(value)
            .map(Value::String)
            .map_err(|e| format!("Failed to decode column with type '{type_name}': {e}")),
    }
}

pub(crate) fn pg_row(row: &PgRow, types: &[String]) -> DecodeResult {
    let mut object = Map::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(index).map_err(|e| e.to_string())?;
        let type_name = types.get(index).map(String::as_str).unwrap_or("");
        let mut value = pg_value(raw, type_name)?;
        parse_json_strings(&mut value);
        object.insert(column.name().to_owned(), value);
    }
    Ok(Value::Object(object))
}

fn pg_as<T>(value: PgValueRef<'_>) -> Result<T, String>
where
    T: for<'r> Decode<'r, Postgres>,
{
    <T as Decode<Postgres>>::decode(value).map_err(|e| e.to_string())
}

fn pg_value(value: PgValueRef<'_>, type_name: &str) -> DecodeResult {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match type_name {
        "BOOL" => pg_as::<bool>(value).map(Value::Bool),
        "INT2" => pg_as::<i16>(value).map(Value::from),
        "INT4" => pg_as::<i32>(value).map(Value::from),
        "INT8" => pg_as::<i64>(value).map(Value::from),
        "OID" => pg_as::<sqlx::postgres::types::Oid>(value).map(|o| Value::from(o.0)),
        "FLOAT4" => pg_as::<f32>(value).map(|v| float(v as f64)),
        "FLOAT8" => pg_as::<f64>(value).map(float),
        "NUMERIC" => pg_as::<rust_decimal::Decimal>(value).map(|d| decimal_string(d.to_string())),
        "JSON" | "JSONB" => pg_as::<Value>(value),
        "UUID" => pg_as::<sqlx::types::Uuid>(value).map(|u| Value::String(u.to_string())),
        "DATE" => pg_as::<chrono::NaiveDate>(value)
            .map(|d| Value::String(d.format("%Y-%m-%d").to_string())),
        "TIME" => pg_as::<chrono::NaiveTime>(value)
            .map(|t| Value::String(t.format("%H:%M:%S").to_string())),
        "TIMESTAMP" => pg_as::<chrono::NaiveDateTime>(value)
            .map(|dt| Value::String(dt.format("%Y-%m-%d %H:%M:%S").to_string())),
        "TIMESTAMPTZ" => {
            pg_as::<chrono::DateTime<chrono::Utc>>(value).map(|dt| Value::String(dt.to_rfc3339()))
        }
        "BYTEA" => pg_as::<Vec<u8>>(value).map(|b| Value::String(STANDARD.encode(b))),
        "INT2[]" => pg_as::<Vec<i16>>(value).map(Value::from),
        "INT4[]" => pg_as::<Vec<i32>>(value).map(Value::from),
        "INT8[]" => pg_as::<Vec<i64>>(value).map(Value::from),
        "FLOAT8[]" => {
            pg_as::<Vec<f64>>(value).map(|v| Value::Array(v.into_iter().map(float).collect()))
        }
        "BOOL[]" => pg_as::<Vec<bool>>(value).map(Value::from),
        "TEXT[]" | "VARCHAR[]" => pg_as::<Vec<String>>(value).map(Value::from),
        "NUMERIC[]" => pg_as::<Vec<rust_decimal::Decimal>>(value).map(|v| {
            Value::Array(
                v.into_iter()
                    .map(|d| decimal_string(d.to_string()))
                    .collect(),
            )
        }),
        // TEXT, VARCHAR, BPCHAR, NAME, CITEXT, enums, ...
        _ => pg_as::<String>(value)
            .map(Value::String)
            .map_err(|e| format!("Failed to decode column with type '{type_name}': {e}")),
    }
}

fn float(v: f64) -> Value {
    Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Exact decimals stay exact when serde_json has `arbitrary_precision` (the FFI build);
/// otherwise they become f64. Unparseable text is returned as a string.
fn decimal_string(s: String) -> Value {
    match serde_json::from_str::<Number>(&s) {
        Ok(n) => Value::Number(n),
        Err(_) => Value::String(s),
    }
}

/// StarRocks returns `to_json(named_struct(...))` / JSON-shaped text as strings; parse them.
pub(crate) fn parse_json_strings(value: &mut Value) {
    match value {
        Value::String(s) => {
            let t = s.trim();
            if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']'))
            {
                if let Ok(parsed) = serde_json::from_str(t) {
                    *value = parsed;
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(parse_json_strings),
        Value::Object(map) => map.values_mut().for_each(parse_json_strings),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_nan_and_infinity_become_null() {
        assert_eq!(float(f64::NAN), Value::Null);
        assert_eq!(float(f64::INFINITY), Value::Null);
        assert_eq!(float(42.5), json!(42.5));
    }

    #[test]
    fn decimal_string_parses_or_falls_back() {
        assert_eq!(decimal_string("12.50".into()).as_f64(), Some(12.5));
        assert_eq!(decimal_string("abc".into()), json!("abc"));
    }

    #[test]
    fn json_strings_are_parsed_recursively() {
        let mut v = json!({"a": "{\"x\":[1,2]}", "b": ["[1]", "plain"], "c": "{not json}"});
        parse_json_strings(&mut v);
        assert_eq!(
            v,
            json!({"a": {"x": [1, 2]}, "b": [[1], "plain"], "c": "{not json}"})
        );
    }
}
