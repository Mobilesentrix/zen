//! Connection map configuration (JSON over the FFI, or built directly in Rust).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

/// Rows one query may return unless the connection sets `maxRows`.
pub const DEFAULT_MAX_ROWS: usize = 10_000;

/// Items one loop-mode database node may query unless the connection sets `maxLoopItems`.
pub const DEFAULT_MAX_LOOP_ITEMS: usize = 1_000;

/// Top-level database configuration.
///
/// ```json
/// {
///   "connections": {
///     "ddd3956b-...": { "url": "mysql://user:pass@starrocks:9030/db", "maxConnections": 16 },
///     "pg-main":      { "url": "postgres://user:pass@pg:5432/app" }
///   },
///   "default": "ddd3956b-...",
///   "workerThreads": 2
/// }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabaseConfig {
    /// instance id (the editor's `instance` field) -> connection settings.
    #[serde(default)]
    pub connections: BTreeMap<String, ConnectionConfig>,
    /// Instance used when a node has no `instance` (or there is only one connection).
    #[serde(default)]
    pub default: Option<String>,
    /// Threads of the dedicated database I/O runtime (default 2).
    #[serde(default)]
    pub worker_threads: Option<usize>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionConfig {
    /// `mysql://`, `mariadb://`, `starrocks://` (MySQL protocol), `postgres://`, `postgresql://`.
    pub url: String,
    /// Extra names that resolve to this connection (e.g. the editor's `instanceName`).
    #[serde(default)]
    pub aliases: Vec<String>,
    pub max_connections: Option<u32>,
    pub min_connections: Option<u32>,
    /// Milliseconds.
    pub acquire_timeout: Option<u64>,
    /// Milliseconds.
    pub idle_timeout: Option<u64>,
    /// Milliseconds.
    pub max_lifetime: Option<u64>,
    /// Prepared statements cached per connection (sqlx default 100).
    pub statement_cache_capacity: Option<usize>,
    /// Open `minConnections` (at least one) at engine creation and fail if unreachable.
    /// Default false: pools are created lazily so one unreachable instance does not stop
    /// rules that never touch it.
    #[serde(default)]
    pub connect_eagerly: bool,
    /// Rows one query may return before it fails (default [`DEFAULT_MAX_ROWS`], minimum 1).
    pub max_rows: Option<usize>,
    /// Items one loop-mode node may run its query for before it fails (default
    /// [`DEFAULT_MAX_LOOP_ITEMS`], minimum 1).
    pub max_loop_items: Option<usize>,
    /// Milliseconds one statement may run before it fails; unset = unbounded.
    pub query_timeout: Option<u64>,
    /// Ping a pooled connection before handing it out (sqlx default true). `false` saves one
    /// round trip per query; keep `idleTimeout` below the server's idle disconnect then.
    pub test_before_acquire: Option<bool>,
    /// `SET <name> = <value>` run on every new connection (e.g. StarRocks
    /// `enable_short_circuit: "true"`). Names must be identifiers; plain integers and
    /// decimals and `true`/`false`/`on`/`off` are sent bare, anything else as a string literal.
    #[serde(default)]
    pub session_variables: BTreeMap<String, String>,
    /// Share one round trip between identical queries inside one evaluate call (default
    /// true; needs the embedder's evaluation scope, see `with_evaluation_scope`). Never
    /// keeps rows beyond that call.
    pub dedupe: Option<bool>,
}

impl fmt::Debug for ConnectionConfig {
    /// Prints the URL without its userinfo so a `{:?}` in a log line cannot leak a password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("url", &redacted_url(&self.url))
            .field("aliases", &self.aliases)
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("statement_cache_capacity", &self.statement_cache_capacity)
            .field("connect_eagerly", &self.connect_eagerly)
            .field("max_rows", &self.max_rows)
            .field("max_loop_items", &self.max_loop_items)
            .field("query_timeout", &self.query_timeout)
            .field("test_before_acquire", &self.test_before_acquire)
            .field("session_variables", &self.session_variables)
            .field("dedupe", &self.dedupe)
            .finish()
    }
}

/// `SET name = value` statements for `sessionVariables`, validated so a value can never
/// change the statement's shape (names are identifiers, text values are quoted literals).
pub(crate) fn session_statements(
    name: &str,
    backend: Backend,
    variables: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    variables
        .iter()
        .map(|(variable, value)| {
            if !is_session_variable_name(variable) {
                return Err(format!(
                    "connection \"{name}\": sessionVariables name \"{variable}\" is not an identifier"
                ));
            }
            let literal = session_value_literal(backend, value);
            Ok(format!("SET {variable} = {literal}"))
        })
        .collect()
}

fn is_session_variable_name(variable: &str) -> bool {
    variable
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && variable
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Plain numbers and `true`/`false`/`on`/`off` are sent bare, anything else as a string literal.
fn session_value_literal(backend: Backend, value: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    if is_plain_number(value) || matches!(lowered.as_str(), "true" | "false" | "on" | "off") {
        value.to_string()
    } else {
        string_literal(backend, value)
    }
}

/// `-12`, `3`, `0.25`: digits with an optional sign and one decimal point (no exponent, no
/// `inf`/`NaN`, which a server may read as an identifier).
fn is_plain_number(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
    !whole.is_empty()
        && !fraction.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.bytes().all(|b| b.is_ascii_digit())
}

/// A quoted string literal: MySQL (and StarRocks) read backslash escapes inside quotes,
/// PostgreSQL's standard strings do not, so only MySQL doubles backslashes.
fn string_literal(backend: Backend, value: &str) -> String {
    let quoted = value.replace('\'', "''");
    match backend {
        Backend::MySql => format!("'{}'", quoted.replace('\\', "\\\\")),
        Backend::Postgres => format!("'{quoted}'"),
    }
}

fn redacted_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.rsplit_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None => "***".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    MySql,
    Postgres,
}

impl ConnectionConfig {
    pub fn backend(&self) -> Option<Backend> {
        let scheme = self.url.split("://").next()?.to_ascii_lowercase();
        match scheme.as_str() {
            "mysql" | "mariadb" | "starrocks" => Some(Backend::MySql),
            "postgres" | "postgresql" => Some(Backend::Postgres),
            _ => None,
        }
    }

    /// URL normalised for sqlx (`starrocks://` -> `mysql://`).
    pub(crate) fn sqlx_url(&self) -> String {
        match self.url.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("starrocks") => {
                format!("mysql://{rest}")
            }
            _ => self.url.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_variables_become_safe_set_statements() {
        let vars: BTreeMap<String, String> = serde_json::from_value(serde_json::json!({
            "enable_short_circuit": "true",
            "sql_select_limit": "2",
            "application_name": "ms zen's",
            "time_zone": "+00:00"
        }))
        .unwrap();
        assert_eq!(
            session_statements("sr", Backend::MySql, &vars).unwrap(),
            [
                "SET application_name = 'ms zen''s'",
                "SET enable_short_circuit = true",
                "SET sql_select_limit = 2",
                "SET time_zone = '+00:00'"
            ]
        );
        for bad in ["1abc", "a b", "x;DROP", "", "a=b"] {
            let vars = BTreeMap::from([(bad.to_string(), "1".to_string())]);
            let err = session_statements("sr", Backend::MySql, &vars).unwrap_err();
            assert!(err.contains("is not an identifier"), "{bad}: {err}");
        }
    }

    #[test]
    fn session_literals_are_bare_only_for_plain_numbers_and_quoted_per_dialect() {
        let vars: BTreeMap<String, String> = serde_json::from_value(serde_json::json!({
            "a": "-12", "b": "0.25", "c": "1e5", "d": "inf", "e": "NaN", "f": "a\\b'c", "g": "OFF"
        }))
        .unwrap();
        assert_eq!(
            session_statements("sr", Backend::MySql, &vars).unwrap(),
            [
                "SET a = -12",
                "SET b = 0.25",
                "SET c = '1e5'",
                "SET d = 'inf'",
                "SET e = 'NaN'",
                "SET f = 'a\\\\b''c'",
                "SET g = OFF"
            ]
        );
        let pg = session_statements("pg", Backend::Postgres, &vars).unwrap();
        assert_eq!(pg[5], "SET f = 'a\\b''c'");
        assert_eq!(pg[2], "SET c = '1e5'");
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let cfg: DatabaseConfig = serde_json::from_str(
            r#"{"connections":{"sr":{"url":"starrocks://admin:s3cret%21@starrocks-fe:9030/shop?ssl-mode=required"}}}"#,
        )
        .unwrap();
        let conn = &cfg.connections["sr"];
        for text in [format!("{conn:?}"), format!("{cfg:?}")] {
            assert!(!text.contains("s3cret"), "{text}");
            assert!(!text.contains("admin:"), "{text}");
            assert!(text.contains("starrocks-fe:9030/shop"), "{text}");
        }
    }
}
