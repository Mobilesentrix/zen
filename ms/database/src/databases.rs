//! Named connection pools plus the dedicated I/O runtime that drives them.
//!
//! Pools live on a runtime that is always being driven, whatever executor the embedder evaluates
//! on. Queries are spawned onto it and the caller awaits the `JoinHandle`, which is
//! executor-agnostic. Pools are created once per engine.

use crate::bind::BoundQuery;
use crate::config::DatabaseConfig;
use crate::connection::{Connection, ConnectionStats};
use crate::error::DatabaseError;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};
use tokio::task::{JoinError, JoinHandle};

/// How long `Drop` lets the pools close cleanly before the runtime is torn down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// A spawned query whose task is aborted when the awaiting future is dropped, so the pool
/// connection is released instead of held until the server answers.
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
            .field("default", &self.default.as_deref().map(Connection::name))
            .finish()
    }
}

impl Drop for Databases {
    fn drop(&mut self) {
        let connections = self.connections();
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
                if let Some(runtime) = slot.take() {
                    shut_down_detached(runtime, close_all);
                }
            }
        }
    }
}

/// Closing the pools needs the runtime driven, and dropping a `Runtime` inside an async context
/// panics, so both happen on a detached thread and the caller never blocks. The runtime is
/// handed over only once the thread exists, so a failed spawn still shuts it down.
fn shut_down_detached<F>(runtime: Runtime, close_all: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let (sender, receiver) = mpsc::channel::<Runtime>();
    let spawned = std::thread::Builder::new()
        .name("ms-zen-db-shutdown".to_string())
        .spawn(move || {
            if let Ok(runtime) = receiver.recv() {
                // The timer must be created inside the runtime context.
                let _ = runtime
                    .block_on(async { tokio::time::timeout(SHUTDOWN_GRACE, close_all).await });
                runtime.shutdown_timeout(SHUTDOWN_GRACE);
            }
        });
    match spawned {
        Ok(_) => {
            let _ = sender.send(runtime);
        }
        Err(_) => runtime.shutdown_background(),
    }
}

/// The connections of a configuration, before a `Databases` owns them.
struct OpenedConnections {
    by_name: HashMap<String, Arc<Connection>>,
    instances: Vec<String>,
    eager: Vec<Arc<Connection>>,
}

fn open_connections(config: &DatabaseConfig) -> Result<OpenedConnections, DatabaseError> {
    let mut by_name = HashMap::new();
    let mut instances = Vec::new();
    let mut eager = Vec::new();
    for (name, connection_config) in &config.connections {
        let connection = Arc::new(Connection::from_config(name, connection_config)?);
        if connection_config.connect_eagerly {
            eager.push(connection.clone());
        }
        instances.push(name.clone());
        for key in std::iter::once(name).chain(connection_config.aliases.iter()) {
            if by_name.insert(key.clone(), connection.clone()).is_some() {
                return Err(DatabaseError::Config(format!(
                    "connection name or alias \"{key}\" is defined twice"
                )));
            }
        }
    }
    Ok(OpenedConnections {
        by_name,
        instances,
        eager,
    })
}

/// The configured default, or the only connection when there is exactly one.
fn default_connection(
    config: &DatabaseConfig,
    opened: &OpenedConnections,
) -> Result<Option<Arc<Connection>>, DatabaseError> {
    match &config.default {
        Some(name) => opened.by_name.get(name).cloned().map(Some).ok_or_else(|| {
            DatabaseError::Config(format!("default connection \"{name}\" is not defined"))
        }),
        None if opened.instances.len() == 1 => {
            Ok(opened.by_name.get(&opened.instances[0]).cloned())
        }
        None => Ok(None),
    }
}

impl Databases {
    /// Parses the JSON configuration and builds the pools on a dedicated runtime.
    ///
    /// # Errors
    /// The JSON or a connection in it is invalid, or a `connectEagerly` connection is unreachable.
    pub fn from_json(json: &str) -> Result<Self, DatabaseError> {
        let config: DatabaseConfig =
            serde_json::from_str(json).map_err(|e| DatabaseError::Config(e.to_string()))?;
        Self::new(config)
    }

    /// Builds pools on a dedicated multi-thread runtime owned by this value.
    ///
    /// # Errors
    /// A connection is invalid, the runtime cannot start, or a `connectEagerly` connection is
    /// unreachable.
    pub fn new(config: DatabaseConfig) -> Result<Self, DatabaseError> {
        let threads = config.worker_threads.unwrap_or(2).max(1);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("ms-zen-db")
            .enable_all()
            .build()
            .map_err(|e| DatabaseError::Config(format!("failed to start database runtime: {e}")))?;
        let handle = runtime.handle().clone();
        Self::build(&config, RuntimeHolder::Owned(Some(runtime)), handle)
    }

    /// Builds pools on a runtime the embedder already runs. The handle must belong to a
    /// multi-thread runtime that keeps being driven for as long as this lives; with
    /// `connectEagerly` on a runtime nobody drives, construction would wait forever.
    ///
    /// # Errors
    /// A connection is invalid, or a `connectEagerly` connection is unreachable.
    pub fn with_handle(config: DatabaseConfig, handle: Handle) -> Result<Self, DatabaseError> {
        Self::build(&config, RuntimeHolder::Borrowed, handle)
    }

    fn build(
        config: &DatabaseConfig,
        runtime: RuntimeHolder,
        handle: Handle,
    ) -> Result<Self, DatabaseError> {
        let runtime_context = handle.enter();
        let opened = open_connections(config)?;
        let default = default_connection(config, &opened)?;
        drop(runtime_context);

        let OpenedConnections {
            by_name,
            instances,
            eager,
        } = opened;
        let databases = Self {
            runtime,
            handle,
            by_name,
            instances,
            default,
        };
        if !eager.is_empty() {
            databases.block_on_db(async move {
                for connection in eager {
                    connection.ping().await?;
                }
                Ok(())
            })?;
        }
        Ok(databases)
    }

    /// Runs a future on the DB runtime and waits for it from a plain thread (construction).
    fn block_on_db<F>(&self, future: F) -> Result<(), DatabaseError>
    where
        F: Future<Output = Result<(), DatabaseError>> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver
            .recv()
            .map_err(|_| DatabaseError::Connection("database runtime stopped".to_string()))?
    }

    /// Every configured connection, once (aliases not repeated).
    fn connections(&self) -> Vec<Arc<Connection>> {
        self.instances
            .iter()
            .filter_map(|name| self.by_name.get(name).cloned())
            .collect()
    }

    /// Statistics of every configured connection, by name (aliases not repeated).
    pub fn stats(&self) -> Vec<(String, ConnectionStats)> {
        self.instances
            .iter()
            .filter_map(|name| {
                self.by_name
                    .get(name)
                    .map(|connection| (name.clone(), connection.stats()))
            })
            .collect()
    }

    pub fn instances(&self) -> &[String] {
        &self.instances
    }

    /// Resolves a node's `instance` (id or alias); `None` uses the default connection.
    ///
    /// # Errors
    /// The instance is not configured, or none was named and there is no default.
    pub fn resolve(&self, instance: Option<&str>) -> Result<Arc<Connection>, DatabaseError> {
        match instance.filter(|name| !name.is_empty()) {
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
    ///
    /// # Errors
    /// The query failed, timed out, returned too many rows, or its task died.
    pub async fn execute(
        &self,
        connection: &Arc<Connection>,
        query: BoundQuery,
    ) -> Result<Vec<Value>, DatabaseError> {
        let connection = connection.clone();
        AbortOnDrop(
            self.handle
                .spawn(async move { connection.run(query).await }),
        )
        .await
        .map_err(|e| DatabaseError::Query(format!("database task failed: {e}")))?
    }

    /// Opens one connection on every pool.
    ///
    /// # Errors
    /// The first unreachable instance.
    pub async fn ping_all(&self) -> Result<(), DatabaseError> {
        let connections = self.connections();
        self.handle
            .spawn(async move {
                for connection in connections {
                    connection.ping().await?;
                }
                Ok(())
            })
            .await
            .map_err(|e| DatabaseError::Connection(format!("database task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
