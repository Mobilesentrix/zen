//! SQL text for identifiers and the literal values of a Lookup step's builder conditions
//! (StarRocks / MySQL dialect, the only one the editor's builder targets).

use serde_json::{Number, Value};

/// Wraps a SQL identifier (table, column or database name) in backticks, doubling any
/// embedded backtick.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Single-quotes a string literal. MySQL / StarRocks read backslash escapes inside quotes, so
/// the backslash is doubled as well as the quote, and NUL is written as `\0`.
pub(crate) fn quote_sql_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for c in value.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("''"),
            '\0' => quoted.push_str("\\0"),
            other => quoted.push(other),
        }
    }
    quoted.push('\'');
    quoted
}

/// A double-quoted rendering of `text` for error messages, escaped like Go's `%q` so a refused
/// rule reads the same in every module.
pub(crate) fn go_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '\x07' => quoted.push_str("\\a"),
            '\x08' => quoted.push_str("\\b"),
            '\x0c' => quoted.push_str("\\f"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\x0b' => quoted.push_str("\\v"),
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            c if c.is_ascii_control() => quoted.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() && (c as u32) <= 0xffff => {
                quoted.push_str(&format!("\\u{:04x}", c as u32))
            }
            c if c.is_control() => quoted.push_str(&format!("\\U{:08x}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn sql_bool(value: bool) -> &'static str {
    if value {
        "TRUE"
    } else {
        "FALSE"
    }
}

/// A JSON number as SQL text. The value goes through an IEEE double and is written in its
/// shortest round-trip form without an exponent, exactly as the Go module renders it (its JSON
/// decoder reads every number as a float64).
fn sql_number(number: &Number) -> Result<String, String> {
    match number.as_f64().filter(|value| value.is_finite()) {
        Some(value) => Ok(format!("{value}")),
        None => Err(format!("number {number} is out of range")),
    }
}

/// `-?digits(.digits)?`: values written unquoted. Date strings such as `2024-01-01` do not match.
fn is_numeric_text(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    all_digits(whole) && fraction.is_none_or(all_digits)
}

/// Formats a condition value for a column of the declared `valueType` (string | number | date |
/// boolean). The declared type wins over the value's own form: a numeric-looking string in a
/// string column is still quoted, and a string in a number column is written bare only when it
/// is a number (anything else is an error, never SQL text). An empty or unknown `valueType` falls
/// back to the value's own form. `Ok(None)` means there is no usable value (absent, empty
/// string, wrong kind): the caller drops the condition.
pub(crate) fn format_typed(
    raw: Option<&Value>,
    value_type: &str,
) -> Result<Option<String>, String> {
    match value_type.trim().to_lowercase().as_str() {
        "number" => format_number(raw),
        "boolean" => Ok(format_boolean(raw)),
        "string" | "date" => format_text(raw),
        _ => format_untyped(raw),
    }
}

fn format_number(raw: Option<&Value>) -> Result<Option<String>, String> {
    match raw {
        Some(Value::Number(number)) => sql_number(number).map(Some),
        Some(Value::Bool(value)) => Ok(Some(sql_bool(*value).to_string())),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else if is_numeric_text(trimmed) {
                Ok(Some(trimmed.to_string()))
            } else {
                Err(format!("value {} is not a number", go_quote(text)))
            }
        }
        _ => Ok(None),
    }
}

fn format_boolean(raw: Option<&Value>) -> Option<String> {
    match raw {
        Some(Value::Bool(value)) => Some(sql_bool(*value).to_string()),
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" => Some("TRUE".to_string()),
            "false" => Some("FALSE".to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn format_text(raw: Option<&Value>) -> Result<Option<String>, String> {
    match raw {
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(quote_sql_string(text))),
        Some(Value::Number(number)) => Ok(Some(quote_sql_string(&sql_number(number)?))),
        _ => Ok(None),
    }
}

/// Quotes by the value's own form: booleans become TRUE/FALSE, numbers (and numeric strings)
/// are bare, every other string is quoted.
fn format_untyped(raw: Option<&Value>) -> Result<Option<String>, String> {
    match raw {
        Some(Value::Bool(value)) => Ok(Some(sql_bool(*value).to_string())),
        Some(Value::Number(number)) => sql_number(number).map(Some),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else if is_numeric_text(trimmed) {
                Ok(Some(trimmed.to_string()))
            } else {
                Ok(Some(quote_sql_string(text)))
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// StarRocks reads MySQL string literals: both the quote and the backslash escape must be
    /// neutralised, otherwise `\'` closes the literal early.
    #[test]
    fn quote_sql_string_neutralises_every_escape() {
        for (input, want) in [
            ("O'Brien", "'O''Brien'"),
            ("\\' OR 1=1 -- ", "'\\\\'' OR 1=1 -- '"),
            ("C:\\new\\table", "'C:\\\\new\\\\table'"),
            ("a\0b", "'a\\0b'"),
            ("plain", "'plain'"),
            ("100%", "'100%'"),
            ("back\\\\slash", "'back\\\\\\\\slash'"),
            ("\"double\" is fine", "'\"double\" is fine'"),
        ] {
            assert_eq!(quote_sql_string(input), want, "{input:?}");
        }
    }

    #[test]
    fn quote_ident_doubles_backticks() {
        assert_eq!(quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn numbers_render_like_go_float64() {
        let cases = [
            (json!(5), "5"),
            (json!(12.5), "12.5"),
            (json!(-1.5), "-1.5"),
            (json!(1e21), "1000000000000000000000"),
            (json!(1e-7), "0.0000001"),
            (json!(0.1), "0.1"),
        ];
        for (value, want) in cases {
            assert_eq!(
                format_untyped(Some(&value)).unwrap().unwrap(),
                want,
                "{value}"
            );
        }
        let big: Value = serde_json::from_str("12.50").unwrap();
        assert_eq!(format_untyped(Some(&big)).unwrap().unwrap(), "12.5");
    }

    #[test]
    fn numeric_text_is_digits_with_one_optional_fraction() {
        for ok in ["0", "-7", "2.5", "-0.25", "007"] {
            assert!(is_numeric_text(ok), "{ok}");
        }
        for bad in [
            "",
            "-",
            "1.",
            ".5",
            "1e5",
            "2024-01-01",
            "1 OR 1=1",
            "+1",
            "1.2.3",
        ] {
            assert!(!is_numeric_text(bad), "{bad}");
        }
    }
}
