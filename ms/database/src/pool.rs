//! Named connection pools plus the dedicated I/O runtime that drives them.
//!
//! The engine evaluates on whatever executor the embedder uses (the C FFI evaluates inside
//! `block_on` on many caller threads; batch evaluation uses a pool of thread-local runtimes;
//! Rust embedders bring their own). sqlx sockets are registered with the reactor of the
//! runtime they were opened on, so pools live on a runtime that is always being driven.
//! Queries are spawned onto it and the caller awaits the `JoinHandle`, which is
//! executor-agnostic. Pools are created once per engine.

use crate::bind::{BindValue, BoundQuery};
use crate::config::{
    session_statements, Backend, ConnectionConfig, DatabaseConfig, DEFAULT_MAX_LOOP_ITEMS,
    DEFAULT_MAX_ROWS,
};
use crate::decode;
use futures_util::future::BoxFuture;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use sqlx::mysql::{
    MySqlArguments, MySqlConnectOptions, MySqlConnection, MySqlPool, MySqlPoolOptions,
};
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Arguments, ConnectOptions, Executor};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, Runtime};
use tokio::task::{JoinError, JoinHandle};

/// How long `Drop` lets the pools close cleanly before the runtime is torn down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DatabaseError {
    #[error("Invalid database configuration: {0}")]
    Config(String),
    #[error("Database connection error: {0}")]
    Connection(String),
    #[error("Query execution error: {0}")]
    Query(String),
    #[error("{0}")]
    UnknownInstance(String),
    #[error("query returned more than {limit} rows (connection \"{connection}\" maxRows); let the query filter, join, sort and LIMIT in the database so it returns only the rows the rule uses. Raising maxRows moves the whole result into the engine and can exhaust its memory")]
    TooManyRows { connection: String, limit: usize },
    #[error("loop of {items} items exceeds maxLoopItems ({limit}) of connection \"{connection}\"; query the items in one statement (for example `IN ({{{{ items }}}})`) or raise maxLoopItems")]
    TooManyLoopItems {
        connection: String,
        limit: usize,
        items: usize,
    },
    #[error("query exceeded the queryTimeout of {ms} ms on connection \"{connection}\"")]
    Timeout { connection: String, ms: u64 },
}

#[derive(Debug, Clone, Copy)]
pub enum PoolKind {
    MySql,
    Postgres,
}

enum Pool {
    MySql(MySqlPool),
    Postgres(PgPool),
}

/// Which server speaks MySQL protocol on a connection. StarRocks' prepared-statement reader
/// accepts integer, float/double and string parameter types but not NEWDECIMAL (type code
/// 246, "unknown mysql type code 246"), so decimals are bound differently per flavor. The
/// flavor cannot come from the URL: production configures StarRocks as `mysql://…:9030`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    MySql,
    StarRocks,
}

pub struct Connection {
    name: String,
    pool: Pool,
    max_rows: usize,
    max_loop_items: usize,
    query_timeout: Option<Duration>,
    dedupe: bool,
    /// Last failure of the connection's `sessionVariables` statements, so a pool timeout caused
    /// by a rejected variable names the cause instead of "pool timed out".
    session_error: Arc<Mutex<Option<String>>>,
    /// Detected on the first MySQL-protocol connection the pool opens (see `detect_flavor`).
    flavor: Arc<OnceLock<Flavor>>,
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the URL (it carries credentials).
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
        let c = &self.stats;
        ConnectionStats {
            queries: c.queries.load(Ordering::Relaxed),
            rows: c.rows.load(Ordering::Relaxed),
            acquire: Duration::from_nanos(c.acquire_ns.load(Ordering::Relaxed)),
            execute: Duration::from_nanos(c.execute_ns.load(Ordering::Relaxed)),
        }
    }

    fn record(
        &self,
        acquired: Instant,
        started: Instant,
        result: &Result<Vec<Value>, DatabaseError>,
    ) {
        let c = &self.stats;
        c.queries.fetch_add(1, Ordering::Relaxed);
        if let Ok(rows) = result {
            c.rows.fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        c.acquire_ns
            .fetch_add(nanos(started - acquired), Ordering::Relaxed);
        c.execute_ns
            .fetch_add(nanos(started.elapsed()), Ordering::Relaxed);
    }

    pub fn kind(&self) -> PoolKind {
        match self.pool {
            Pool::MySql(_) => PoolKind::MySql,
            Pool::Postgres(_) => PoolKind::Postgres,
        }
    }

    /// Runs one bound query on a connection taken from the pool, bounded by `queryTimeout`
    /// when configured. A timed-out connection still has the statement in flight on the
    /// server, so it is detached and dropped instead of returned (the pool opens a
    /// replacement); the next query must not inherit the wait.
    async fn run(&self, q: BoundQuery) -> Result<Vec<Value>, DatabaseError> {
        let begin = Instant::now();
        let mut started = begin;
        let result = self.run_inner(q, &mut started).await;
        self.record(begin, started, &result);
        result
    }

    async fn run_inner(
        &self,
        q: BoundQuery,
        started: &mut Instant,
    ) -> Result<Vec<Value>, DatabaseError> {
        let acquire_err = |e: sqlx::Error| match self
            .session_error
            .lock()
            .ok()
            .and_then(|cause| cause.clone())
        {
            Some(cause) => DatabaseError::Connection(format!(
                "{}: {cause} (no connection could be opened: {e})",
                self.name
            )),
            None => DatabaseError::Query(e.to_string()),
        };
        match &self.pool {
            Pool::MySql(pool) => {
                // acquire first: opening the connection is what detects the flavor
                let mut conn = pool.acquire().await.map_err(acquire_err)?;
                *started = Instant::now();
                let flavor = self.flavor.get().copied().unwrap_or(Flavor::MySql);
                let args = mysql_args(&q.params, flavor)?;
                let stream = sqlx::query_with(&q.sql, args).fetch(&mut *conn);
                match self.bounded(self.collect(stream, decode::mysql_row)).await {
                    Ok(result) => result,
                    Err(timeout) => {
                        drop(conn.detach());
                        Err(timeout)
                    }
                }
            }
            Pool::Postgres(pool) => {
                let args = pg_args(&q.params)?;
                let mut conn = pool.acquire().await.map_err(acquire_err)?;
                *started = Instant::now();
                let stream = sqlx::query_with(&q.sql, args).fetch(&mut *conn);
                match self.bounded(self.collect(stream, decode::pg_row)).await {
                    Ok(result) => result,
                    Err(timeout) => {
                        drop(conn.detach());
                        Err(timeout)
                    }
                }
            }
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
                ms: limit.as_millis() as u64,
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

    async fn ping(&self) -> Result<(), DatabaseError> {
        let conn_err = |e: sqlx::Error| DatabaseError::Connection(format!("{}: {e}", self.name));
        match &self.pool {
            Pool::MySql(p) => p.acquire().await.map(drop).map_err(conn_err),
            Pool::Postgres(p) => p.acquire().await.map(drop).map_err(conn_err),
        }
    }

    async fn close(&self) {
        match &self.pool {
            Pool::MySql(p) => p.close().await,
            Pool::Postgres(p) => p.close().await,
        }
    }
}

/// Binds the parameters for a MySQL-protocol statement. Decimals go to MySQL/MariaDB as
/// NEWDECIMAL (exact, index-friendly). StarRocks rejects that parameter type, so there a
/// decimal is sent as its plain text ("12.5"), which StarRocks folds into an exact decimal
/// literal of the column's type (`cost = 12.5`, not `CAST(cost AS DOUBLE) = 12.5`); a DOUBLE
/// parameter would cast the column and lose both precision and pruning. A bool goes to MySQL
/// as TINYINT UNSIGNED, a parameter type StarRocks also rejects ("unknown mysql type code
/// 32769"), so there it is sent as the integer 1 / 0, which StarRocks reads as true / false.
fn mysql_args(params: &[BindValue], flavor: Flavor) -> Result<MySqlArguments, DatabaseError> {
    let mut args = MySqlArguments::default();
    for p in params {
        let r = match p {
            BindValue::Bool(v) => match flavor {
                Flavor::MySql => args.add(*v),
                Flavor::StarRocks => args.add(i64::from(*v)),
            },
            BindValue::Int(v) => args.add(*v),
            BindValue::Decimal(v) => match flavor {
                Flavor::MySql => args.add(*v),
                Flavor::StarRocks => args.add(v.to_string()),
            },
            BindValue::Text(v) => args.add(v.as_str()),
        };
        r.map_err(|e| DatabaseError::Query(e.to_string()))?;
    }
    Ok(args)
}

fn pg_args(params: &[BindValue]) -> Result<PgArguments, DatabaseError> {
    let mut args = PgArguments::default();
    for p in params {
        let r = match p {
            BindValue::Bool(v) => args.add(*v),
            BindValue::Int(v) => args.add(*v),
            BindValue::Decimal(v) => args.add(*v),
            BindValue::Text(v) => args.add(v.as_str()),
        };
        r.map_err(|e| DatabaseError::Query(e.to_string()))?;
    }
    Ok(args)
}

/// A spawned query whose task is aborted when the awaiting future is dropped (batch worker
/// cancelled, caller timed out), so the pool connection is released instead of held until
/// the server answers.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum RuntimeHolder {
    Owned(Option<Runtime>),
    Borrowed,
}

/// All configured connections. Construct once per engine and share (`Arc`).
pub struct Databases {
    runtime: RuntimeHolder,
    handle: Handle,
    by_name: HashMap<String, Arc<Connection>>,
    instances: Vec<String>,
    default: Option<Arc<Connection>>,
}

impl fmt::Debug for Databases {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Databases")
            .field("instances", &self.instances)
            .field("default", &self.default.as_ref().map(|c| c.name.clone()))
            .finish()
    }
}

impl Drop for Databases {
    fn drop(&mut self) {
        let connections: Vec<Arc<Connection>> = self
            .instances
            .iter()
            .filter_map(|name| self.by_name.get(name).cloned())
            .collect();
        let close_all = async move {
            for connection in connections {
                connection.close().await;
            }
        };
        match &mut self.runtime {
            RuntimeHolder::Borrowed => {
                self.handle.spawn(close_all);
            }
            RuntimeHolder::Owned(slot) => {
                let Some(runtime) = slot.take() else {
                    return;
                };
                // Closing the pools needs the runtime driven, and dropping a Runtime inside an
                // async context panics, so both happen on a detached thread; the caller (often
                // `zen_engine_free`) never blocks. The runtime is handed over only once the
                // thread exists, so a failed spawn still shuts it down.
                let (tx, rx) = std::sync::mpsc::channel::<Runtime>();
                let spawned = std::thread::Builder::new()
                    .name("ms-zen-db-shutdown".to_string())
                    .spawn(move || {
                        if let Ok(runtime) = rx.recv() {
                            // The timer must be created inside the runtime context, so the
                            // timeout is built inside the async block.
                            let _ = runtime.block_on(async {
                                tokio::time::timeout(SHUTDOWN_GRACE, close_all).await
                            });
                            runtime.shutdown_timeout(SHUTDOWN_GRACE);
                        }
                    });
                match spawned {
                    Ok(_) => {
                        let _ = tx.send(runtime);
                    }
                    Err(_) => runtime.shutdown_background(),
                }
            }
        }
    }
}

impl Databases {
    /// Parses the JSON configuration and builds the pools on a dedicated runtime.
    pub fn from_json(json: &str) -> Result<Self, DatabaseError> {
        let config: DatabaseConfig =
            serde_json::from_str(json).map_err(|e| DatabaseError::Config(e.to_string()))?;
        Self::new(config)
    }

    /// Builds pools on a dedicated multi-thread runtime owned by this value.
    pub fn new(config: DatabaseConfig) -> Result<Self, DatabaseError> {
        let threads = config.worker_threads.unwrap_or(2).max(1);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("ms-zen-db")
            .enable_all()
            .build()
            .map_err(|e| DatabaseError::Config(format!("failed to start database runtime: {e}")))?;
        let handle = runtime.handle().clone();
        Self::build(config, RuntimeHolder::Owned(Some(runtime)), handle)
    }

    /// Builds pools on a runtime the embedder already runs. The handle must belong to a
    /// multi-thread runtime that keeps being driven for as long as this lives; with
    /// `connectEagerly` on a runtime nobody drives, construction would wait forever.
    pub fn with_handle(config: DatabaseConfig, handle: Handle) -> Result<Self, DatabaseError> {
        Self::build(config, RuntimeHolder::Borrowed, handle)
    }

    fn build(
        config: DatabaseConfig,
        runtime: RuntimeHolder,
        handle: Handle,
    ) -> Result<Self, DatabaseError> {
        let _guard = handle.enter();
        let mut by_name = HashMap::new();
        let mut instances = Vec::new();
        let mut eager = Vec::new();
        for (name, conn_cfg) in &config.connections {
            let session_error = Arc::new(Mutex::new(None));
            let flavor = Arc::new(OnceLock::new());
            let conn = Arc::new(Connection {
                name: name.clone(),
                pool: make_pool(name, conn_cfg, session_error.clone(), flavor.clone())?,
                max_rows: max_rows(name, conn_cfg)?,
                max_loop_items: max_loop_items(name, conn_cfg)?,
                query_timeout: conn_cfg.query_timeout.map(Duration::from_millis),
                dedupe: conn_cfg.dedupe.unwrap_or(true),
                session_error,
                flavor,
                stats: Counters::default(),
            });
            if conn_cfg.connect_eagerly {
                eager.push(conn.clone());
            }
            instances.push(name.clone());
            for key in std::iter::once(name).chain(conn_cfg.aliases.iter()) {
                if by_name.insert(key.clone(), conn.clone()).is_some() {
                    return Err(DatabaseError::Config(format!(
                        "connection name or alias \"{key}\" is defined twice"
                    )));
                }
            }
        }
        let default = match &config.default {
            Some(name) => Some(by_name.get(name).cloned().ok_or_else(|| {
                DatabaseError::Config(format!("default connection \"{name}\" is not defined"))
            })?),
            None if instances.len() == 1 => by_name.get(&instances[0]).cloned(),
            None => None,
        };
        drop(_guard);

        let databases = Self {
            runtime,
            handle,
            by_name,
            instances,
            default,
        };
        if !eager.is_empty() {
            databases.block_on_db(async move {
                for conn in eager {
                    conn.ping().await?;
                }
                Ok(())
            })?;
        }
        Ok(databases)
    }

    /// Runs a future on the DB runtime and waits for it from a plain thread (construction).
    fn block_on_db<F>(&self, fut: F) -> Result<(), DatabaseError>
    where
        F: Future<Output = Result<(), DatabaseError>> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv()
            .map_err(|_| DatabaseError::Connection("database runtime stopped".to_string()))?
    }

    /// Statistics of every configured connection, by name (aliases not repeated).
    pub fn stats(&self) -> Vec<(String, ConnectionStats)> {
        self.instances
            .iter()
            .filter_map(|n| self.by_name.get(n).map(|c| (n.clone(), c.stats())))
            .collect()
    }

    pub fn instances(&self) -> &[String] {
        &self.instances
    }

    /// Resolves a node's `instance` (id or alias); `None` uses the default connection.
    pub fn resolve(&self, instance: Option<&str>) -> Result<Arc<Connection>, DatabaseError> {
        match instance.filter(|s| !s.is_empty()) {
            Some(name) => self.by_name.get(name).cloned().ok_or_else(|| {
                DatabaseError::UnknownInstance(format!(
                    "database instance \"{name}\" is not configured (configured: {})",
                    self.instances.join(", ")
                ))
            }),
            None => self.default.clone().ok_or_else(|| {
                DatabaseError::UnknownInstance(format!(
                    "node has no database instance and no default connection is configured (configured: {})",
                    self.instances.join(", ")
                ))
            }),
        }
    }

    /// Executes on the DB runtime; awaitable from any executor. Dropping the returned future
    /// aborts the query task.
    pub async fn execute(
        &self,
        conn: &Arc<Connection>,
        query: BoundQuery,
    ) -> Result<Vec<Value>, DatabaseError> {
        let conn = conn.clone();
        AbortOnDrop(self.handle.spawn(async move { conn.run(query).await }))
            .await
            .map_err(|e| DatabaseError::Query(format!("database task failed: {e}")))?
    }

    /// Opens one connection on every pool (fails on the first unreachable instance).
    pub async fn ping_all(&self) -> Result<(), DatabaseError> {
        let conns: Vec<Arc<Connection>> = self
            .instances
            .iter()
            .filter_map(|n| self.by_name.get(n).cloned())
            .collect();
        self.handle
            .spawn(async move {
                for c in conns {
                    c.ping().await?;
                }
                Ok(())
            })
            .await
            .map_err(|e| DatabaseError::Connection(format!("database task failed: {e}")))?
    }
}

fn max_rows(name: &str, cfg: &ConnectionConfig) -> Result<usize, DatabaseError> {
    match cfg.max_rows {
        Some(0) => Err(DatabaseError::Config(format!(
            "connection \"{name}\": maxRows must be at least 1"
        ))),
        Some(n) => Ok(n),
        None => Ok(DEFAULT_MAX_ROWS),
    }
}

fn max_loop_items(name: &str, cfg: &ConnectionConfig) -> Result<usize, DatabaseError> {
    match cfg.max_loop_items {
        Some(0) => Err(DatabaseError::Config(format!(
            "connection \"{name}\": maxLoopItems must be at least 1"
        ))),
        Some(n) => Ok(n),
        None => Ok(DEFAULT_MAX_LOOP_ITEMS),
    }
}

fn make_pool(
    name: &str,
    cfg: &ConnectionConfig,
    session_error: Arc<Mutex<Option<String>>>,
    flavor: Arc<OnceLock<Flavor>>,
) -> Result<Pool, DatabaseError> {
    let backend = cfg.backend().ok_or_else(|| {
        let scheme = cfg.url.split("://").next().unwrap_or("");
        DatabaseError::Config(format!(
            "connection \"{name}\": unsupported database scheme '{scheme}' (use mysql://, starrocks://, mariadb://, postgres://)"
        ))
    })?;
    let url = cfg.sqlx_url();
    let session: Arc<Vec<String>> = Arc::new(
        session_statements(name, backend, &cfg.session_variables).map_err(DatabaseError::Config)?,
    );
    let bad_url =
        |e: sqlx::Error| DatabaseError::Config(format!("connection \"{name}\": invalid url: {e}"));
    match backend {
        Backend::MySql => {
            // StarRocks rejects the non-constant `SET sql_mode=(SELECT CONCAT(...))` sqlx sends
            // on connect, so leave sql_mode alone.
            let mut opts = MySqlConnectOptions::from_str(&url)
                .map_err(bad_url)?
                .pipes_as_concat(false)
                .no_engine_substitution(false)
                .disable_statement_logging();
            if let Some(cap) = cfg.statement_cache_capacity {
                opts = opts.statement_cache_capacity(cap);
            }
            let pool = pool_options::<sqlx::MySql>(MySqlPoolOptions::new(), cfg)
                .after_connect(move |conn, _| {
                    let flavor = flavor.clone();
                    let session = session.clone();
                    let session_error = session_error.clone();
                    Box::pin(async move {
                        detect_flavor(conn, &flavor).await?;
                        run_session(conn, session, session_error).await
                    })
                })
                .connect_lazy_with(opts);
            Ok(Pool::MySql(pool))
        }
        Backend::Postgres => {
            let mut opts = PgConnectOptions::from_str(&url)
                .map_err(bad_url)?
                .disable_statement_logging();
            if let Some(cap) = cfg.statement_cache_capacity {
                opts = opts.statement_cache_capacity(cap);
            }
            let pool = pool_options::<sqlx::Postgres>(PgPoolOptions::new(), cfg)
                .after_connect(move |conn, _| {
                    run_session(conn, session.clone(), session_error.clone())
                })
                .connect_lazy_with(opts);
            Ok(Pool::Postgres(pool))
        }
    }
}

/// Runs the connection's `SET` statements (simple-query protocol, no prepare) on a new
/// connection; a failure makes the connect fail, so a bad variable surfaces on first use.
/// Decides once per pool whether the MySQL-protocol server is StarRocks: `current_version()`
/// is a StarRocks function ("3.5.20-4d17879"); MySQL and MariaDB answer with an "unknown
/// function" database error. Any other failure (I/O) fails the connection as usual. One
/// extra round trip per new connection, none per query.
async fn detect_flavor(
    conn: &mut MySqlConnection,
    flavor: &OnceLock<Flavor>,
) -> Result<(), sqlx::Error> {
    if flavor.get().is_some() {
        return Ok(());
    }
    let detected = match sqlx::query_scalar::<_, String>("SELECT current_version()")
        .fetch_one(&mut *conn)
        .await
    {
        Ok(_) => Flavor::StarRocks,
        Err(sqlx::Error::Database(_)) => Flavor::MySql,
        Err(e) => return Err(e),
    };
    let _ = flavor.set(detected);
    Ok(())
}

fn run_session<'c, C>(
    conn: &'c mut C,
    statements: Arc<Vec<String>>,
    session_error: Arc<Mutex<Option<String>>>,
) -> BoxFuture<'c, Result<(), sqlx::Error>>
where
    for<'e> &'e mut C: Executor<'e>,
    C: Send,
{
    Box::pin(async move {
        for statement in statements.iter() {
            if let Err(e) = conn.execute(statement.as_str()).await {
                if let Ok(mut cause) = session_error.lock() {
                    *cause = Some(format!(
                        "session variable statement `{statement}` failed: {e}"
                    ));
                }
                return Err(e);
            }
        }
        if let Ok(mut cause) = session_error.lock() {
            *cause = None;
        }
        Ok(())
    })
}

fn pool_options<DB: sqlx::Database>(
    mut o: sqlx::pool::PoolOptions<DB>,
    cfg: &ConnectionConfig,
) -> sqlx::pool::PoolOptions<DB> {
    if let Some(v) = cfg.max_connections {
        o = o.max_connections(v);
    }
    if let Some(v) = cfg.min_connections {
        o = o.min_connections(v);
    }
    if let Some(v) = cfg.acquire_timeout {
        o = o.acquire_timeout(Duration::from_millis(v));
    }
    if let Some(v) = cfg.idle_timeout {
        o = o.idle_timeout(Duration::from_millis(v));
    }
    if let Some(v) = cfg.max_lifetime {
        o = o.max_lifetime(Duration::from_millis(v));
    }
    if let Some(v) = cfg.test_before_acquire {
        o = o.test_before_acquire(v);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn errors_never_echo_credentials() {
        let leaked = |err: String| err.contains("s3cret") || err.contains("user:");
        // eager connect to a closed port
        let err = Databases::from_json(
            r#"{"connections":{"x":{"url":"mysql://user:s3cret@127.0.0.1:1/db","connectEagerly":true,"acquireTimeout":500}}}"#,
        )
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(!leaked(err.clone()), "{err}");
        assert!(err.contains("Database connection error: x:"), "{err}");
        // malformed URLs carrying a password
        for url in [
            "mysql://user:s3cret@h:notaport/db",
            "postgres://user:s3cret@h:notaport/db",
            "mysql://user:s3cret@h/db?ssl-mode=bogus",
        ] {
            let json = format!(r#"{{"connections":{{"x":{{"url":"{url}"}}}}}}"#);
            let err = Databases::from_json(&json)
                .map(|_| ())
                .unwrap_err()
                .to_string();
            assert!(!leaked(err.clone()), "{url}: {err}");
        }
        // lazy pool: a query against an unreachable host fails at execution time
        let dbs = Databases::from_json(
            r#"{"connections":{"x":{"url":"postgres://user:s3cret@127.0.0.1:1/db","acquireTimeout":500}}}"#,
        )
        .unwrap();
        let conn = dbs.resolve(None).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let query = BoundQuery {
            sql: "SELECT 1".into(),
            params: Vec::new(),
        };
        let err = rt
            .block_on(dbs.execute(&conn, query))
            .unwrap_err()
            .to_string();
        assert!(!leaked(err.clone()), "{err}");
    }

    #[test]
    fn errors_name_the_connection_and_the_limit() {
        let e = DatabaseError::TooManyRows {
            connection: "sr".into(),
            limit: 10,
        };
        assert_eq!(
            e.to_string(),
            "query returned more than 10 rows (connection \"sr\" maxRows); let the query filter, join, sort and LIMIT in the database so it returns only the rows the rule uses. Raising maxRows moves the whole result into the engine and can exhaust its memory"
        );
        let e = DatabaseError::Timeout {
            connection: "sr".into(),
            ms: 300,
        };
        assert_eq!(
            e.to_string(),
            "query exceeded the queryTimeout of 300 ms on connection \"sr\""
        );
    }
}
