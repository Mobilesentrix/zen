use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use mysql::{MySqlAdapter, MySqlAdapterBuilder};
pub use noop::NoopDatabaseAdapter;

mod mysql;
mod noop;

pub type DynamicDatabaseAdapter = Arc<dyn DatabaseAdapter + Send + Sync>;
pub type DatabaseResult = Result<DatabaseResponse, DatabaseError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseRequest {
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseResponse {
    pub rows: Value,
    pub row_count: usize,
}

/// Trait used for implementing a database adapter
pub trait DatabaseAdapter: Debug + Send + Sync {
    fn execute<'a>(
        &'a self,
        request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>>;
}

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
