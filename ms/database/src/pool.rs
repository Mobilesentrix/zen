//! Creating the sqlx pools of a connection and preparing each connection they open.

use crate::config::{session_statements, Backend, ConnectionConfig};
use crate::error::DatabaseError;
use futures_util::future::BoxFuture;
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlPool, MySqlPoolOptions};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Executor};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub(crate) enum Pool {
    MySql(MySqlPool),
    Postgres(PgPool),
}

/// Which server speaks MySQL protocol on a connection; it decides how decimals and bools are
/// bound. It cannot come from the URL: StarRocks may be configured as `mysql://…:9030`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    MySql,
    StarRocks,
}

/// Last failure of a connection's `sessionVariables` statements.
pub(crate) type SessionError = Arc<Mutex<Option<String>>>;

/// Detected on the first MySQL-protocol connection a pool opens.
pub(crate) type SharedFlavor = Arc<OnceLock<Flavor>>;

/// Builds the lazy pool of one configured connection.
pub(crate) fn make_pool(
    name: &str,
    config: &ConnectionConfig,
    session_error: SessionError,
    flavor: SharedFlavor,
) -> Result<Pool, DatabaseError> {
    let backend = config.backend().ok_or_else(|| {
        let scheme = config.url.split("://").next().unwrap_or("");
        DatabaseError::Config(format!(
            "connection \"{name}\": unsupported database scheme '{scheme}' (use mysql://, starrocks://, mariadb://, postgres://)"
        ))
    })?;
    let session = Arc::new(
        session_statements(name, backend, &config.session_variables)
            .map_err(DatabaseError::Config)?,
    );
    match backend {
        Backend::MySql => mysql_pool(name, config, session, session_error, flavor),
        Backend::Postgres => postgres_pool(name, config, session, session_error),
    }
}

fn invalid_url(name: &str, error: &sqlx::Error) -> DatabaseError {
    DatabaseError::Config(format!("connection \"{name}\": invalid url: {error}"))
}

/// Leaves `sql_mode` alone: StarRocks rejects the non-constant `SET sql_mode=(SELECT CONCAT(...))`
/// sqlx would send on connect.
fn mysql_pool(
    name: &str,
    config: &ConnectionConfig,
    session: Arc<Vec<String>>,
    session_error: SessionError,
    flavor: SharedFlavor,
) -> Result<Pool, DatabaseError> {
    let mut options = MySqlConnectOptions::from_str(&config.sqlx_url())
        .map_err(|e| invalid_url(name, &e))?
        .pipes_as_concat(false)
        .no_engine_substitution(false)
        .disable_statement_logging();
    if let Some(capacity) = config.statement_cache_capacity {
        options = options.statement_cache_capacity(capacity);
    }
    let pool = pool_options::<sqlx::MySql>(MySqlPoolOptions::new(), config)
        .after_connect(move |connection, _| {
            let flavor = flavor.clone();
            let session = session.clone();
            let session_error = session_error.clone();
            Box::pin(async move {
                detect_flavor(connection, &flavor).await?;
                run_session(connection, session, session_error).await
            })
        })
        .connect_lazy_with(options);
    Ok(Pool::MySql(pool))
}

fn postgres_pool(
    name: &str,
    config: &ConnectionConfig,
    session: Arc<Vec<String>>,
    session_error: SessionError,
) -> Result<Pool, DatabaseError> {
    let mut options = PgConnectOptions::from_str(&config.sqlx_url())
        .map_err(|e| invalid_url(name, &e))?
        .disable_statement_logging();
    if let Some(capacity) = config.statement_cache_capacity {
        options = options.statement_cache_capacity(capacity);
    }
    let pool = pool_options::<sqlx::Postgres>(PgPoolOptions::new(), config)
        .after_connect(move |connection, _| {
            run_session(connection, session.clone(), session_error.clone())
        })
        .connect_lazy_with(options);
    Ok(Pool::Postgres(pool))
}

/// Decides once per pool whether the MySQL-protocol server is StarRocks: `current_version()` is a
/// StarRocks function; MySQL and MariaDB answer with an "unknown function" database error. Any
/// other failure (I/O) fails the connection as usual.
async fn detect_flavor(
    connection: &mut MySqlConnection,
    flavor: &OnceLock<Flavor>,
) -> Result<(), sqlx::Error> {
    if flavor.get().is_some() {
        return Ok(());
    }
    let detected = match sqlx::query_scalar::<_, String>("SELECT current_version()")
        .fetch_one(&mut *connection)
        .await
    {
        Ok(_) => Flavor::StarRocks,
        Err(sqlx::Error::Database(_)) => Flavor::MySql,
        Err(e) => return Err(e),
    };
    let _ = flavor.set(detected);
    Ok(())
}

/// Runs the connection's `SET` statements (simple-query protocol, no prepare) on a new
/// connection; a failure makes the connect fail, so a bad variable surfaces on first use.
fn run_session<'c, C>(
    connection: &'c mut C,
    statements: Arc<Vec<String>>,
    session_error: SessionError,
) -> BoxFuture<'c, Result<(), sqlx::Error>>
where
    for<'e> &'e mut C: Executor<'e>,
    C: Send,
{
    Box::pin(async move {
        for statement in statements.iter() {
            if let Err(e) = connection.execute(statement.as_str()).await {
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
    mut options: sqlx::pool::PoolOptions<DB>,
    config: &ConnectionConfig,
) -> sqlx::pool::PoolOptions<DB> {
    if let Some(value) = config.max_connections {
        options = options.max_connections(value);
    }
    if let Some(value) = config.min_connections {
        options = options.min_connections(value);
    }
    if let Some(value) = config.acquire_timeout {
        options = options.acquire_timeout(Duration::from_millis(value));
    }
    if let Some(value) = config.idle_timeout {
        options = options.idle_timeout(Duration::from_millis(value));
    }
    if let Some(value) = config.max_lifetime {
        options = options.max_lifetime(Duration::from_millis(value));
    }
    if let Some(value) = config.test_before_acquire {
        options = options.test_before_acquire(value);
    }
    options
}
