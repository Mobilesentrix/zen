use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type DynamicDatabaseAdapter = Arc<dyn DatabaseAdapter>;

/// A database query request passed from the engine to the adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseRequest {
    pub query: String,
    // #[serde(default)] // No need as we will always receive consturcted SELECT query
    // pub params: Vec<Value>,
    // #[serde(default)]
    // pub connection: Option<String>, // Only needed when multiple databases injected
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseResponse {
    pub rows: Value,
    pub row_count: usize,
}

/// Errors that can occur during database operations.
#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("Database connection error: {0}")]
    Connection(String),

    #[error("Query execution error: {0}")]
    Query(String),

    #[error("Database adapter not configured")]
    NotConfigured,

    #[error("{0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

pub type DatabaseResult = Result<DatabaseResponse, DatabaseError>;

/// Abstract trait for database backends.
///
/// Implementations must be `Send + Sync` (they live inside `Arc` and are shared
/// across async tasks) and `Debug` (for logging / error messages).
///
/// # Example
///
/// ```ignore
/// #[derive(Debug)]
/// struct MySqlAdapter { pool: sqlx::MySqlPool }
///
/// impl DatabaseAdapter for MySqlAdapter {
///     fn execute<'a>(
///         &'a self,
///         request: &'a DatabaseRequest,
///     ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
///         Box::pin(async move {
///             let rows = sqlx::query(&request.query)
///                 .fetch_all(&self.pool)
///                 .await
///                 .map_err(|e| DatabaseError::Query(e.to_string()))?;
///
///             Ok(DatabaseResponse { rows: todo!(), row_count: rows.len() as usize })
///         })
///     }
/// }
/// ```

pub trait DatabaseAdapter: Debug + Send + Sync {
    /// Execute a query against the database and return the result rows.
    fn execute<'a>(
        &'a self,
        request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>>;
}

/// Default adapter used when no real database is wired up.
#[derive(Debug, Default)]
pub struct NoopDatabaseAdapter;

impl DatabaseAdapter for NoopDatabaseAdapter {
    fn execute<'a>(
        &'a self,
        _request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        Box::pin(async { Err(DatabaseError::NotConfigured) })
    }
}
