//! What a node does with the rows its query returned (`resultHandling`, MS-FFI.md §3).

use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use zen_expression::Variable;

/// Rows a result-handling error keeps in its details (the offending rows for "several").
pub const DETAIL_ROWS: usize = 10;

/// The node's `resultHandling`; the defaults output the plain `{rows, rowsCount}`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResultHandling {
    #[serde(default)]
    pub none: NoneHandling,
    /// Message of the node error when `none` is `error` (default "Not found").
    #[serde(default)]
    pub none_message: Option<String>,
    #[serde(default)]
    pub one: OneHandling,
    /// `many` is accepted as an alias.
    #[serde(default, alias = "many")]
    pub several: SeveralHandling,
    /// Message of the node error when `several` is `error` (default "Duplicates found").
    #[serde(default, alias = "manyMessage")]
    pub several_message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoneHandling {
    /// Default: `{rows: [], rowsCount: 0}`.
    #[default]
    Continue,
    /// Fail the node with `noneMessage`.
    Error,
    /// Write `{rowsCount: 0}` (every column reads as null downstream).
    Empty,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OneHandling {
    /// Default: `{rows: [row], rowsCount: 1}`.
    #[default]
    All,
    /// Write the row object itself plus `rowsCount: 1`.
    Use,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeveralHandling {
    /// Default: `{rows: [...], rowsCount: n}`.
    #[default]
    All,
    /// Fail the node with `severalMessage`.
    Error,
    /// Write row 0 as an object plus `rowsCount: n`.
    First,
}

/// A node error raised by `resultHandling`: `Display` is the analyst's message, `details()` the
/// structured part (`reason`, `rowsCount`, and for "several" up to [`DETAIL_ROWS`] rows).
#[derive(Debug, Clone, PartialEq)]
pub struct ResultHandlingError {
    message: String,
    reason: &'static str,
    rows_count: usize,
    rows: Vec<Value>,
}

impl ResultHandlingError {
    pub fn none(message: String) -> Self {
        Self {
            message,
            reason: "none",
            rows_count: 0,
            rows: Vec::new(),
        }
    }

    pub fn several(message: String, rows_count: usize, mut rows: Vec<Value>) -> Self {
        rows.truncate(DETAIL_ROWS);
        Self {
            message,
            reason: "several",
            rows_count,
            rows,
        }
    }

    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn rows_count(&self) -> usize {
        self.rows_count
    }

    pub fn details(&self) -> Value {
        let mut details = Map::with_capacity(3);
        details.insert("reason".to_string(), Value::from(self.reason));
        details.insert("rowsCount".to_string(), Value::from(self.rows_count));
        if self.reason == "several" {
            details.insert("rows".to_string(), Value::Array(self.rows.clone()));
        }
        Value::Object(details)
    }
}

impl fmt::Display for ResultHandlingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ResultHandlingError {}

fn message_or(configured: Option<&str>, default: &str) -> String {
    match configured.map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => default.to_string(),
    }
}

/// The plain output object, `{rows, rowsCount}`.
fn raw_result(rows: Vec<Value>) -> Variable {
    let count = rows.len();
    let mut result = Map::with_capacity(2);
    result.insert("rows".to_string(), Value::Array(rows));
    result.insert("rowsCount".to_string(), Value::from(count));
    Variable::from(Value::Object(result))
}

/// One row as the output object, with `rowsCount` added (it overwrites a column of that name).
fn row_with_count(mut row: Value, count: usize) -> Variable {
    if let Value::Object(object) = &mut row {
        object.insert("rowsCount".to_string(), Value::from(count));
    }
    Variable::from(row)
}

/// Applies `resultHandling` to the fetched rows: `(output, trace)`. The trace is always the raw
/// `{rows, rowsCount}`; with the defaults the output is that same object (shared).
pub(crate) fn shape_result(
    rows: Vec<Value>,
    handling: &ResultHandling,
) -> Result<(Variable, Variable), ResultHandlingError> {
    let count = rows.len();
    let shaped = match count {
        0 => match handling.none {
            NoneHandling::Continue => None,
            NoneHandling::Error => {
                return Err(ResultHandlingError::none(message_or(
                    handling.none_message.as_deref(),
                    "Not found",
                )));
            }
            NoneHandling::Empty => Some(row_with_count(Value::Object(Map::new()), 0)),
        },
        1 => match handling.one {
            OneHandling::All => None,
            OneHandling::Use => Some(row_with_count(rows[0].clone(), 1)),
        },
        _ => match handling.several {
            SeveralHandling::All => None,
            SeveralHandling::Error => {
                return Err(ResultHandlingError::several(
                    message_or(handling.several_message.as_deref(), "Duplicates found"),
                    count,
                    rows,
                ));
            }
            SeveralHandling::First => Some(row_with_count(rows[0].clone(), count)),
        },
    };
    let trace = raw_result(rows);
    Ok(match shaped {
        Some(output) => (output, trace),
        None => (trace.shallow_clone(), trace),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(n: usize) -> Vec<Value> {
        (1..=n)
            .map(|i| json!({"id": i, "sku": format!("S{i}")}))
            .collect()
    }

    fn handling(v: Value) -> ResultHandling {
        serde_json::from_value(v).unwrap()
    }

    fn shaped(rows: Vec<Value>, h: &ResultHandling) -> Result<(Value, Value), ResultHandlingError> {
        shape_result(rows, h).map(|(out, trace)| {
            (
                serde_json::to_value(&out).unwrap(),
                serde_json::to_value(&trace).unwrap(),
            )
        })
    }

    #[test]
    fn result_handling_default_keeps_todays_shape() {
        let h = ResultHandling::default();
        assert_eq!(h, handling(json!({})));
        assert_eq!(
            h,
            handling(json!({"none": "continue", "one": "all", "several": "all"}))
        );
        for n in [0, 1, 3] {
            let (out, trace) = shaped(rows(n), &h).unwrap();
            assert_eq!(out, json!({"rows": rows(n), "rowsCount": n}));
            assert_eq!(trace, out);
        }
    }

    #[test]
    fn result_handling_none() {
        let (out, _) = shaped(rows(0), &handling(json!({"none": "empty"}))).unwrap();
        assert_eq!(out, json!({"rowsCount": 0}));
        let e = shaped(rows(0), &handling(json!({"none": "error"}))).unwrap_err();
        assert_eq!(e.to_string(), "Not found");
        assert_eq!(e.details(), json!({"reason": "none", "rowsCount": 0}));
        let e = shaped(
            rows(0),
            &handling(json!({"none": "error", "noneMessage": "No such product"})),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "No such product");
        // blank message falls back to the default
        let e = shaped(
            rows(0),
            &handling(json!({"none": "error", "noneMessage": "  "})),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "Not found");
        // none-handling does not touch one/several
        let (out, _) = shaped(rows(1), &handling(json!({"none": "error"}))).unwrap();
        assert_eq!(out, json!({"rows": rows(1), "rowsCount": 1}));
    }

    #[test]
    fn result_handling_one_use_writes_the_row_object() {
        let h = handling(json!({"one": "use"}));
        let (out, trace) = shaped(rows(1), &h).unwrap();
        assert_eq!(out, json!({"id": 1, "sku": "S1", "rowsCount": 1}));
        assert_eq!(
            trace,
            json!({"rows": rows(1), "rowsCount": 1}),
            "trace keeps the raw result"
        );
        // a column literally named rowsCount is overwritten by the count (documented)
        let (out, _) = shape_result(vec![json!({"rowsCount": "x", "a": 1})], &h).unwrap();
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({"rowsCount": 1, "a": 1})
        );
        // zero or several rows are unaffected by `one`
        let (out, _) = shaped(rows(2), &h).unwrap();
        assert_eq!(out, json!({"rows": rows(2), "rowsCount": 2}));
    }

    #[test]
    fn result_handling_several() {
        let (out, _) = shaped(rows(3), &handling(json!({"several": "first"}))).unwrap();
        assert_eq!(out, json!({"id": 1, "sku": "S1", "rowsCount": 3}));
        let e = shaped(rows(3), &handling(json!({"several": "error"}))).unwrap_err();
        assert_eq!(e.to_string(), "Duplicates found");
        assert_eq!(
            e.details(),
            json!({"reason": "several", "rowsCount": 3, "rows": rows(3)})
        );
        let e = shaped(
            rows(3),
            &handling(json!({"several": "error", "severalMessage": "More than one match"})),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "More than one match");
        // the offending rows in the details are capped
        let e = shaped(
            rows(DETAIL_ROWS + 5),
            &handling(json!({"several": "error"})),
        )
        .unwrap_err();
        assert_eq!(e.details()["rows"].as_array().unwrap().len(), DETAIL_ROWS);
        assert_eq!(e.details()["rowsCount"], json!(DETAIL_ROWS + 5));
        // `many` / `manyMessage` are accepted as aliases
        assert_eq!(
            handling(json!({"many": "first", "manyMessage": "m"})),
            handling(json!({"several": "first", "severalMessage": "m"}))
        );
    }
}
