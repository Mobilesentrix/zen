use crate::{DatabaseAdapter, DatabaseError, DatabaseRequest, DatabaseResponse, DatabaseResult};
use serde_json::{Map, Number, Value};
use sqlx::mysql::{
    MySqlArguments, MySqlPool, MySqlPoolOptions, MySqlRow, MySqlTypeInfo, MySqlValueRef,
};
use sqlx::{Column, Decode, MySql, Row, TypeInfo, ValueRef};
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct MySqlAdapter {
    pool: MySqlPool,
}

impl MySqlAdapter {
    pub async fn connect(database_url: &str) -> Result<Self, DatabaseError> {
        let pool = MySqlPool::connect(database_url)
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
            let rows = rows
                .into_iter()
                .map(row_to_json)
                .collect::<Result<Vec<_>, _>>()?;

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
        let mut options = MySqlPoolOptions::new();

        if let Some(max) = self.max_connections {
            options = options.max_connections(max);
        }
        if let Some(min) = self.min_connections {
            options = options.min_connections(min);
        }
        if let Some(timeout) = self.acquire_timeout {
            options = options.acquire_timeout(timeout);
        }
        if let Some(timeout) = self.idle_timeout {
            options = options.idle_timeout(timeout);
        }
        if let Some(lifetime) = self.max_lifetime {
            options = options.max_lifetime(lifetime);
        }

        let pool = options
            .connect(database_url)
            .await
            .map_err(|err| DatabaseError::Connection(err.to_string()))?;

        Ok(MySqlAdapter { pool })
    }
}

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

    if matches!(
        type_name.as_str(),
        "BOOL" | "BOOLEAN" // type_name == "TINYINT" also possible as boolean: mysql doesn't have native boolean type.
    ) {
        return decode_as::<bool>(value).map(Value::Bool);
    }

    if type_name.contains("INT") {
        if type_name.contains("UNSIGNED") {
            return decode_as::<u64>(value).map(|v| Value::Number(Number::from(v)));
        }

        return decode_as::<i64>(value).map(|v| Value::Number(Number::from(v)));
    }

    if matches!(type_name.as_str(), "FLOAT" | "DOUBLE" | "DECIMAL") {
        return decode_as::<f64>(value).map(number_to_json);
    }

    if type_name == "BIT" {
        return decode_as::<u64>(value).map(|v| Value::Number(Number::from(v)));
    }

    if matches!(
        type_name.as_str(),
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB"
    ) {
        return decode_as::<Vec<u8>>(value).map(|bytes| {
            Value::Array(
                bytes
                    .into_iter()
                    .map(|byte| Value::Number(Number::from(byte)))
                    .collect(),
            )
        });
    }

    // This is the fallback case where all other data types (eg. VARCHAR, TEXT, DATE, DATETIME, etc.) are decoded as stirngs.
    decode_as::<String>(value).map(Value::String)
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
