use crate::options::ConnectionOptions;
use crate::{DatabaseAdapter, DatabaseError, DatabaseRequest, DatabaseResponse, DatabaseResult};
use serde_json::{Map, Number, Value};
use sqlx::mysql::{
    MySqlArguments, MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlRow, MySqlTypeInfo,
    MySqlValueRef,
};
use sqlx::types::chrono;
use sqlx::{Column, Decode, MySql, Row, TypeInfo, ValueRef};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct MySqlAdapter {
    pool: MySqlPool,
}

impl MySqlAdapter {
    pub async fn connect(database_url: &str) -> Result<Self, DatabaseError> {
        // Parse URL into connect options so we can disable the default
        // SET sql_mode=(SELECT CONCAT(@@sql_mode, ',...')) statement that
        // sqlx sends on connect. Databases like StarRocks reject non-constant
        // expressions inside SET and fail with error 1064.
        let options = MySqlConnectOptions::from_str(database_url)
            .map_err(|err| DatabaseError::Connection(err.to_string()))?
            .pipes_as_concat(false)
            .no_engine_substitution(false);

        let pool = MySqlPool::connect_with(options)
            .await
            .map_err(|err| DatabaseError::Connection(err.to_string()))?;

        Ok(Self { pool })
    }

    pub fn builder() -> MySqlAdapterBuilder {
        MySqlAdapterBuilder::default()
    }

    pub fn get_pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub async fn close_connection(&self) {
        self.pool.close().await;
    }
}

impl DatabaseAdapter for MySqlAdapter {
    fn execute<'a>(
        &'a self,
        request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        Box::pin(async move {
            let query: sqlx::query::Query<'_, MySql, MySqlArguments> = sqlx::query(&request.query);

            // No need for v1 SELECT query
            // for param in &request.params {
            //     query = bind_json_value(query, param);
            // }

            let rows = query
                .fetch_all(&self.pool)
                .await
                .map_err(|err| DatabaseError::Query(err.to_string()))?;

            let row_count = rows.len() as usize;
            let mut rows = rows
                .into_iter()
                .map(row_to_json)
                .collect::<Result<Vec<_>, _>>()?;

            // Parsing the json string to json object for joins or named structs in query results
            for row in rows.iter_mut() {
                parse_json_strings(row);
            }

            Ok(DatabaseResponse {
                rows: Value::Array(rows),
                row_count,
            })
        })
    }
}

#[derive(Debug, Default)]
pub struct MySqlAdapterBuilder {
    max_connections: Option<u32>,
    min_connections: Option<u32>,
    acquire_timeout: Option<std::time::Duration>,
    idle_timeout: Option<std::time::Duration>,
    max_lifetime: Option<std::time::Duration>,
    test_before_acquire: Option<bool>,
}

impl MySqlAdapterBuilder {
    pub fn max_connections(mut self, max: u32) -> Self {
        self.max_connections = Some(max);
        self
    }

    pub fn min_connections(mut self, min: u32) -> Self {
        self.min_connections = Some(min);
        self
    }

    pub fn acquire_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.acquire_timeout = Some(timeout);
        self
    }

    pub fn idle_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }

    pub fn max_lifetime(mut self, lifetime: std::time::Duration) -> Self {
        self.max_lifetime = Some(lifetime);
        self
    }

    pub fn test_before_acquire(mut self, test: bool) -> Self {
        self.test_before_acquire = Some(test);
        self
    }

    pub async fn connect(self, database_url: &str) -> Result<MySqlAdapter, DatabaseError> {
        let mut pool_options = MySqlPoolOptions::new();

        if let Some(max) = self.max_connections {
            pool_options = pool_options.max_connections(max);
        }
        if let Some(min) = self.min_connections {
            pool_options = pool_options.min_connections(min);
        }
        if let Some(timeout) = self.acquire_timeout {
            pool_options = pool_options.acquire_timeout(timeout);
        }
        if let Some(timeout) = self.idle_timeout {
            pool_options = pool_options.idle_timeout(timeout);
        }
        if let Some(lifetime) = self.max_lifetime {
            pool_options = pool_options.max_lifetime(lifetime);
        }

        // Disable the default SET sql_mode=(SELECT CONCAT(...)) statement for StarRocks compatibility.
        let connect_options = MySqlConnectOptions::from_str(database_url)
            .map_err(|err| DatabaseError::Connection(err.to_string()))?
            .pipes_as_concat(false)
            .no_engine_substitution(false);

        let pool = pool_options
            .connect_with(connect_options)
            .await
            .map_err(|err| DatabaseError::Connection(err.to_string()))?;

        Ok(MySqlAdapter { pool })
    }
}

impl From<ConnectionOptions> for MySqlAdapterBuilder {
    fn from(opts: ConnectionOptions) -> Self {
        let mut builder = MySqlAdapterBuilder::default();
        if let Some(v) = opts.max_connections {
            builder = builder.max_connections(v);
        }
        if let Some(v) = opts.min_connections {
            builder = builder.min_connections(v);
        }
        if let Some(v) = opts.acquire_timeout {
            builder = builder.acquire_timeout(v);
        }
        if let Some(v) = opts.idle_timeout {
            builder = builder.idle_timeout(v);
        }
        if let Some(v) = opts.max_lifetime {
            builder = builder.max_lifetime(v);
        }
        builder
    }
}

// Required when the params are passed in query
#[allow(dead_code)]
fn bind_json_value<'q>(
    query: sqlx::query::Query<'q, MySql, MySqlArguments>,
    value: &Value,
) -> sqlx::query::Query<'q, MySql, MySqlArguments> {
    match value {
        Value::Null => query.bind(Option::<String>::None),
        Value::Bool(value) => query.bind(*value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                query.bind(value)
            } else if let Some(value) = value.as_u64() {
                query.bind(value)
            } else if let Some(value) = value.as_f64() {
                query.bind(value)
            } else {
                query.bind(value.to_string())
            }
        }
        Value::String(value) => query.bind(value.clone()),
        Value::Array(_) | Value::Object(_) => query.bind(value.to_string()),
    }
}

fn row_to_json(row: MySqlRow) -> Result<Value, DatabaseError> {
    let mut object = Map::with_capacity(row.len());

    for (index, column) in row.columns().iter().enumerate() {
        let value = decode_mysql_value(row.try_get_raw(index), column.type_info())?;
        object.insert(column.name().to_owned(), value);
    }

    Ok(Value::Object(object))
}

fn decode_mysql_value(
    value: Result<MySqlValueRef<'_>, sqlx::Error>,
    type_info: &MySqlTypeInfo,
) -> Result<Value, DatabaseError> {
    let value = value.map_err(|err| DatabaseError::Query(err.to_string()))?;
    if value.is_null() {
        return Ok(Value::Null);
    }

    let type_name = type_info.name().to_ascii_uppercase();

    if type_name == "JSON" {
        return decode_as::<Value>(value);
    }

    if type_name.contains("BINARY")
        || type_name.contains("BLOB")
        || type_name.contains("GEOMETRY")
        || matches!(
            type_name.as_str(),
            "POINT" | "MULTIPOINT" | "LINESTRING" | "MULTILINESTRING" | "POLYGON" | "MULTIPOLYGON"
        )
    {
        return decode_as::<Vec<u8>>(value).map(|bytes| {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            Value::String(STANDARD.encode(&bytes))
        });
    }

    if type_name == "BIT" {
        return decode_as::<u64>(value).map(|v| Value::Number(Number::from(v)));
    }

    if matches!(
        type_name.as_str(),
        "TINYINT"
            | "TINYINT UNSIGNED"
            | "SMALLINT"
            | "SMALLINT UNSIGNED"
            | "MEDIUMINT"
            | "MEDIUMINT UNSIGNED"
            | "INT"
            | "INT UNSIGNED"
            | "BIGINT"
            | "BIGINT UNSIGNED"
    ) {
        if type_name.contains("UNSIGNED") {
            return decode_as::<u64>(value).map(|v| Value::Number(Number::from(v)));
        }
        return decode_as::<i64>(value).map(|v| Value::Number(Number::from(v)));
    }

    if matches!(type_name.as_str(), "FLOAT" | "DOUBLE") {
        return decode_as::<f64>(value).map(number_to_json);
    }

    if matches!(type_name.as_str(), "DECIMAL" | "NUMERIC" | "NEWDECIMAL") {
        return decode_as::<String>(value).map(|s| {
            s.parse::<f64>()
                .map(number_to_json)
                .unwrap_or(Value::String(s))
        });
    }

    if matches!(type_name.as_str(), "BOOL" | "BOOLEAN") {
        return decode_as::<bool>(value).map(Value::Bool);
    }

    // Temporal types (binary protocol sends raw bytes, not text)
    if type_name == "DATE" {
        return decode_as::<chrono::NaiveDate>(value)
            .map(|d| Value::String(d.format("%Y-%m-%d").to_string()));
    }

    if type_name == "TIME" {
        return decode_as::<chrono::NaiveTime>(value)
            .map(|t| Value::String(t.format("%H:%M:%S").to_string()));
    }

    if matches!(type_name.as_str(), "DATETIME" | "TIMESTAMP") {
        return decode_as::<chrono::NaiveDateTime>(value)
            .map(|dt| Value::String(dt.format("%Y-%m-%d %H:%M:%S").to_string()));
    }

    if type_name == "YEAR" {
        return decode_as::<u16>(value).map(|y| Value::Number(Number::from(y)));
    }

    // Fallback: text-like types (VARCHAR, TEXT, CHAR, ENUM, SET, etc.)
    match <String as Decode<MySql>>::decode(value) {
        Ok(s) => Ok(Value::String(s)),
        Err(err) => Err(DatabaseError::Query(format!(
            "Failed to decode column with type '{}': {}",
            type_name, err
        ))),
    }
}

fn decode_as<T>(value: MySqlValueRef<'_>) -> Result<T, DatabaseError>
where
    T: for<'r> Decode<'r, MySql>,
{
    <T as Decode<MySql>>::decode(value).map_err(|err| DatabaseError::Query(err.to_string()))
}

fn number_to_json(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn parse_json_strings(val: &mut Value) {
    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(parsed) = serde_json::from_str(s) {
                    *val = parsed;
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                parse_json_strings(item);
            }
        }
        Value::Object(obj) => {
            for (_, value) in obj.iter_mut() {
                parse_json_strings(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_to_json_normal() {
        assert_eq!(number_to_json(42.5), json!(42.5));
    }

    #[test]
    fn number_to_json_zero() {
        assert_eq!(number_to_json(0.0), json!(0.0));
    }

    #[test]
    fn number_to_json_nan_returns_null() {
        assert_eq!(number_to_json(f64::NAN), Value::Null);
    }

    #[test]
    fn number_to_json_infinity_returns_null() {
        assert_eq!(number_to_json(f64::INFINITY), Value::Null);
    }

    #[test]
    fn number_to_json_neg_infinity_returns_null() {
        assert_eq!(number_to_json(f64::NEG_INFINITY), Value::Null);
    }

    #[test]
    fn number_to_json_large_number() {
        assert_eq!(number_to_json(1e308), json!(1e308));
    }

    #[test]
    fn builder_default_has_none_values() {
        let builder = MySqlAdapterBuilder::default();
        assert!(builder.max_connections.is_none());
        assert!(builder.min_connections.is_none());
        assert!(builder.acquire_timeout.is_none());
        assert!(builder.idle_timeout.is_none());
        assert!(builder.max_lifetime.is_none());
        assert!(builder.test_before_acquire.is_none());
    }

    #[test]
    fn builder_chain_sets_values() {
        let builder = MySqlAdapter::builder()
            .max_connections(10)
            .min_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .idle_timeout(std::time::Duration::from_secs(60))
            .max_lifetime(std::time::Duration::from_secs(3600))
            .test_before_acquire(true);

        assert_eq!(builder.max_connections, Some(10));
        assert_eq!(builder.min_connections, Some(2));
        assert_eq!(
            builder.acquire_timeout,
            Some(std::time::Duration::from_secs(5))
        );
        assert!(builder.test_before_acquire.unwrap());
    }
}
