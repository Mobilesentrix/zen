use crate::{DatabaseAdapter, DatabaseError, DatabaseRequest, DatabaseResult};
use std::pin::Pin;

#[derive(Debug, Default)]
pub struct NoopDatabaseAdapter;

impl DatabaseAdapter for NoopDatabaseAdapter {
    fn execute<'a>(
        &'a self,
        _request: &'a DatabaseRequest,
    ) -> Pin<Box<dyn Future<Output = DatabaseResult> + Send + 'a>> {
        Box::pin(async move { Err(DatabaseError::NotConfigured) })
    }
}
