#![deny(clippy::unwrap_used)]
#![allow(clippy::module_inception)]

use std::sync::Arc;

mod adapter;
mod options;

pub use adapter::*;
pub use options::*;

/// This function is only meant to be used from bindings.
#[cfg(feature = "bindings")]
pub async fn connect(url: &str) -> Result<DynamicDatabaseAdapter, DatabaseError> {
    connect_with_options(url, ConnectionOptions::default()).await
}

/// This function is only meant to be used from bindings.
#[cfg(feature = "bindings")]
pub async fn connect_with_options(
    url: &str,
    options: ConnectionOptions,
) -> Result<DynamicDatabaseAdapter, DatabaseError> {
    let scheme = url.split("://").next().unwrap_or("");
    match scheme {
        "mysql" | "mariadb" => {
            let adapter = MySqlAdapterBuilder::from(options).connect(url).await?;
            Ok(Arc::new(adapter))
        }
        // "postgres" | "postgresql" => { Ok(Arc::new(PostgresAdapterBuilder::from(options).connect(url).await?)) }
        other => Err(DatabaseError::Connection(format!(
            "Unsupported database scheme: '{other}'"
        ))),
    }
}
