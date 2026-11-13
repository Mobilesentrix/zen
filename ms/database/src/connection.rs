//! One configured connection: its pool, limits and statistics, and running a query on it.

use crate::bind::{BindValue, BoundQuery};
use crate::config::{ConnectionConfig, DEFAULT_MAX_LOOP_ITEMS, DEFAULT_MAX_ROWS};
use crate::decode;
use crate::error::DatabaseError;
use crate::pool::{make_pool, Flavor, Pool, SessionError, SharedFlavor};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use sqlx::mysql::MySqlArguments;
use sqlx::postgres::PgArguments;
use sqlx::Arguments;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub enum PoolKind {
    MySql,
    Postgres,
}

pub struct Connection {
    name: String,
    pool: Pool,
    max_rows: usize,
    max_loop_items: usize,
    query_timeout: Option<Duration>,
    dedupe: bool,
    /// Names the cause when a pool timeout comes from a rejected session variable.
    session_error: SessionError,
    flavor: SharedFlavor,
    stats: Counters,
}

/// Running totals of one connection (relaxed atomics: four adds per query, no lock).
#[derive(Debug, Default)]
struct Counters {
    queries: AtomicU64,
    rows: AtomicU64,
    acquire_ns: AtomicU64,
    execute_ns: AtomicU64,
}

/// What a connection has done since it was created: round trips (queries sent), rows
/// received, and the time spent waiting for a pooled connection vs running statements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionStats {
    pub queries: u64,
    pub rows: u64,
    pub acquire: Duration,
    pub execute: Duration,
}

impl ConnectionStats {
    /// The counts accumulated since `earlier` (a previous snapshot of the same connection).
    pub fn since(&self, earlier: &ConnectionStats) -> ConnectionStats {
        ConnectionStats {
            queries: self.queries - earlier.queries,
            rows: self.rows - earlier.rows,
            acquire: self.acquire.saturating_sub(earlier.acquire),
            execute: self.execute.saturating_sub(earlier.execute),
        }
    }
}

impl fmt::Debug for Connection {
    /// Never prints the URL (it carries credentials).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("name", &self.name)
            .field("kind", &self.kind())
            .field("max_rows", &self.max_rows)
            .field("max_loop_items", &self.max_loop_items)
            .field("query_timeout", &self.query_timeout)
            .field("flavor", &self.flavor.get())
            .finish()
    }
}

impl Connection {
    /// Builds the connection and its lazy pool; must run inside the runtime that drives the pool.
    pub(crate) fn from_config(
        name: &str,
        config: &ConnectionConfig,
    ) -> Result<Self, DatabaseError> {
        let session_error: SessionError = Arc::new(Mutex::new(None));
        let flavor: SharedFlavor = Arc::new(OnceLock::new());
        Ok(Self {
            name: name.to_string(),
            pool: make_pool(name, config, session_error.clone(), flavor.clone())?,
            max_rows: max_rows(name, config)?,
            max_loop_items: max_loop_items(name, config)?,
            query_timeout: config.query_timeout.map(Duration::from_millis),
            dedupe: config.dedupe.unwrap_or(true),
            session_error,
            flavor,
            stats: Counters::default(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Items one loop-mode node may query (`maxLoopItems`).
    pub fn max_loop_items(&self) -> usize {
        self.max_loop_items
    }

    /// A node error for a loop of `items` items over [`Self::max_loop_items`].
    pub fn too_many_loop_items(&self, items: usize) -> DatabaseError {
        DatabaseError::TooManyLoopItems {
            connection: self.name.clone(),
            limit: self.max_loop_items,
            items,
        }
    }

    /// Whether identical queries inside one evaluation scope share a round trip.
    pub fn dedupe(&self) -> bool {
        self.dedupe
    }

    pub fn stats(&self) -> ConnectionStats {
        let counters = &self.stats;
        ConnectionStats {
            queries: counters.queries.load(Ordering::Relaxed),
            rows: counters.rows.load(Ordering::Relaxed),
            acquire: Duration::from_nanos(counters.acquire_ns.load(Ordering::Relaxed)),
            execute: Duration::from_nanos(counters.execute_ns.load(Ordering::Relaxed)),
        }
    }

    fn record(
        &self,
        acquired: Instant,
        started: Instant,
        result: &Result<Vec<Value>, DatabaseError>,
    ) {
        let counters = &self.stats;
        counters.queries.fetch_add(1, Ordering::Relaxed);
        if let Ok(rows) = result {
            counters
                .rows
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        counters.acquire_ns.fetch_add(
            saturating_u64((started - acquired).as_nanos()),
            Ordering::Relaxed,
        );
        counters.execute_ns.fetch_add(
            saturating_u64(started.elapsed().as_nanos()),
            Ordering::Relaxed,
        );
    }

    pub fn kind(&self) -> PoolKind {
        match self.pool {
            Pool::MySql(_) => PoolKind::MySql,
            Pool::Postgres(_) => PoolKind::Postgres,
        }
    }

    /// Runs one bound query on a connection taken from the pool, bounded by `queryTimeout`
    /// when configured.
    pub(crate) async fn run(&self, query: BoundQuery) -> Result<Vec<Value>, DatabaseError> {
        let begin = Instant::now();
        let mut execute_started = begin;
        let result = match &self.pool {
            Pool::MySql(pool) => self.run_mysql(pool, query, &mut execute_started).await,
            Pool::Postgres(pool) => self.run_postgres(pool, query, &mut execute_started).await,
        };
        self.record(begin, execute_started, &result);
        result
    }

    /// Acquires before binding: opening the connection is what detects the flavor. A timed-out
    /// connection still has the statement in flight on the server, so it is detached and dropped
    /// instead of returned (the pool opens a replacement).
    async fn run_mysql(
        &self,
        pool: &sqlx::MySqlPool,
        query: BoundQuery,
        execute_started: &mut Instant,
    ) -> Result<Vec<Value>, DatabaseError> {
        let mut connection = pool.acquire().await.map_err(|e| self.acquire_error(&e))?;
        *execute_started = Instant::now();
        let flavor = self.flavor.get().copied().unwrap_or(Flavor::MySql);
        let arguments = mysql_arguments(&query.params, flavor)?;
        let stream = sqlx::query_with(&query.sql, arguments)
            .persistent(reuses_prepared_statements(flavor))
            .fetch(&mut *connection);
        match self.bounded(self.collect(stream, decode::mysql_row)).await {
            Ok(result) => result,
            Err(timeout) => {
                drop(connection.detach());
                Err(timeout)
            }
        }
    }

    /// A timed-out connection is detached and dropped, as in [`Self::run_mysql`].
    async fn run_postgres(
        &self,
        pool: &sqlx::PgPool,
        query: BoundQuery,
        execute_started: &mut Instant,
    ) -> Result<Vec<Value>, DatabaseError> {
        let arguments = postgres_arguments(&query.params)?;
        let mut connection = pool.acquire().await.map_err(|e| self.acquire_error(&e))?;
        *execute_started = Instant::now();
        let stream = sqlx::query_with(&query.sql, arguments).fetch(&mut *connection);
        match self.bounded(self.collect(stream, decode::pg_row)).await {
            Ok(result) => result,
            Err(timeout) => {
                drop(connection.detach());
                Err(timeout)
            }
        }
    }

    fn acquire_error(&self, error: &sqlx::Error) -> DatabaseError {
        let session_cause = self
            .session_error
            .lock()
            .ok()
            .and_then(|cause| cause.clone());
        match session_cause {
            Some(cause) => DatabaseError::Connection(format!(
                "{}: {cause} (no connection could be opened: {error})",
                self.name
            )),
            None => DatabaseError::Query(error.to_string()),
        }
    }

    /// Applies `queryTimeout` to `work`. The outer `Err` is the timeout, kept apart from the
    /// query's own result so the caller can discard the connection.
    async fn bounded<F>(&self, work: F) -> Result<Result<Vec<Value>, DatabaseError>, DatabaseError>
    where
        F: Future<Output = Result<Vec<Value>, DatabaseError>>,
    {
        let Some(limit) = self.query_timeout else {
            return Ok(work.await);
        };
        match tokio::time::timeout(limit, work).await {
            Ok(result) => Ok(result),
            Err(_) => Err(DatabaseError::Timeout {
                connection: self.name.clone(),
                ms: saturating_u64(limit.as_millis()),
            }),
        }
    }

    /// Streams rows and decodes them one at a time, so a result set larger than `maxRows` is
    /// rejected without ever being materialised.
    async fn collect<R, S, F>(
        &self,
        mut stream: S,
        decode_row: F,
    ) -> Result<Vec<Value>, DatabaseError>
    where
        R: sqlx::Row,
        S: Stream<Item = Result<R, sqlx::Error>> + Unpin,
        F: Fn(&R, &[String]) -> decode::DecodeResult,
    {
        let mut rows = Vec::new();
        let mut types: Option<Vec<String>> = None;
        while let Some(row) = stream.next().await {
            let row = row.map_err(|e| DatabaseError::Query(e.to_string()))?;
            if rows.len() >= self.max_rows {
                return Err(DatabaseError::TooManyRows {
                    connection: self.name.clone(),
                    limit: self.max_rows,
                });
            }
            let types = types.get_or_insert_with(|| decode::column_types(&row));
            rows.push(decode_row(&row, types).map_err(DatabaseError::Query)?);
        }
        Ok(rows)
    }

    pub(crate) async fn ping(&self) -> Result<(), DatabaseError> {
        let connection_error =
            |e: sqlx::Error| DatabaseError::Connection(format!("{}: {e}", self.name));
        match &self.pool {
            Pool::MySql(pool) => pool.acquire().await.map(drop).map_err(connection_error),
            Pool::Postgres(pool) => pool.acquire().await.map(drop).map_err(connection_error),
        }
    }

    pub(crate) async fn close(&self) {
        match &self.pool {
            Pool::MySql(pool) => pool.close().await,
            Pool::Postgres(pool) => pool.close().await,
        }
    }
}

fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Whether a MySQL-protocol connection keeps its prepared statements for re-execution.
/// StarRocks (3.5.20, 4.1.4) re-executes a prepared statement whose plan has a correlated
/// subquery with the parameter values of its first execution, so there every query is prepared,
/// executed and closed (one extra round trip); docs/starrocks-prepared-statement-stale-params.md.
fn reuses_prepared_statements(flavor: Flavor) -> bool {
    match flavor {
        Flavor::MySql => true,
        Flavor::StarRocks => false,
    }
}

/// Binds the parameters for a MySQL-protocol statement. StarRocks' prepared-statement reader
/// rejects NEWDECIMAL and TINYINT UNSIGNED parameters, so there a decimal is sent as its plain
/// text and a bool as the integer 1 / 0; MySQL and MariaDB get the native types.
fn mysql_arguments(params: &[BindValue], flavor: Flavor) -> Result<MySqlArguments, DatabaseError> {
    let mut arguments = MySqlArguments::default();
    for param in params {
        let added = match param {
            BindValue::Bool(value) => match flavor {
                Flavor::MySql => arguments.add(*value),
                Flavor::StarRocks => arguments.add(i64::from(*value)),
            },
            BindValue::Int(value) => arguments.add(*value),
            BindValue::Decimal(value) => match flavor {
                Flavor::MySql => arguments.add(*value),
                Flavor::StarRocks => arguments.add(value.to_string()),
            },
            BindValue::Text(value) => arguments.add(value.as_str()),
        };
        added.map_err(|e| DatabaseError::Query(e.to_string()))?;
    }
    Ok(arguments)
}

fn postgres_arguments(params: &[BindValue]) -> Result<PgArguments, DatabaseError> {
    let mut arguments = PgArguments::default();
    for param in params {
        let added = match param {
            BindValue::Bool(value) => arguments.add(*value),
            BindValue::Int(value) => arguments.add(*value),
            BindValue::Decimal(value) => arguments.add(*value),
            BindValue::Text(value) => arguments.add(value.as_str()),
        };
        added.map_err(|e| DatabaseError::Query(e.to_string()))?;
    }
    Ok(arguments)
}

/// A configured limit, or its default; zero is refused.
fn positive_limit(
    name: &str,
    field: &str,
    configured: Option<usize>,
    default: usize,
) -> Result<usize, DatabaseError> {
    match configured {
        Some(0) => Err(DatabaseError::Config(format!(
            "connection \"{name}\": {field} must be at least 1"
        ))),
        Some(limit) => Ok(limit),
        None => Ok(default),
    }
}

fn max_rows(name: &str, config: &ConnectionConfig) -> Result<usize, DatabaseError> {
    positive_limit(name, "maxRows", config.max_rows, DEFAULT_MAX_ROWS)
}

fn max_loop_items(name: &str, config: &ConnectionConfig) -> Result<usize, DatabaseError> {
    positive_limit(
        name,
        "maxLoopItems",
        config.max_loop_items,
        DEFAULT_MAX_LOOP_ITEMS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use crate::databases::Databases;

    #[test]
    fn max_rows_defaults_and_rejects_zero() {
        let cfg: DatabaseConfig = serde_json::from_str(
            r#"{"connections":{"a":{"url":"mysql://u:p@h/db"},"b":{"url":"mysql://u:p@h/db","maxRows":5}}}"#,
        )
        .unwrap();
        assert_eq!(
            max_rows("a", &cfg.connections["a"]).unwrap(),
            DEFAULT_MAX_ROWS
        );
        assert_eq!(max_rows("b", &cfg.connections["b"]).unwrap(), 5);
        let zero =
            Databases::from_json(r#"{"connections":{"z":{"url":"mysql://u:p@h/db","maxRows":0}}}"#)
                .map(|_| ())
                .unwrap_err();
        assert!(
            zero.to_string().contains("maxRows must be at least 1"),
            "{zero}"
        );
    }
}
