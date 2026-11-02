//! MobileSentrix database node for zen-engine 2.x, implemented as a `CustomNodeAdapter`
//! (no engine-core patches).
//!
//! ```ignore
//! let dbs = Arc::new(Databases::from_json(r#"{"connections":{"sr":{"url":"mysql://..."}}}"#)?);
//! let engine = DecisionEngine::new(loader, Arc::new(DatabaseCustomNode::new(dbs)));
//! ```

mod adapter;
mod bind;
mod config;
mod connection;
mod databases;
mod decode;
mod error;
mod pool;
mod query_step;
mod result_handling;
mod scope;
mod template;

pub use adapter::{DatabaseCustomNode, ExecutionMode, FallbackAdapter, DATABASE_NODE_KIND};
pub use bind::{bind, BindError, BindValue, BoundQuery, NodeLabel, MAX_BIND_PARAMS, MAX_COUNT};
pub use config::{
    Backend, ConnectionConfig, DatabaseConfig, DEFAULT_MAX_LOOP_ITEMS, DEFAULT_MAX_ROWS,
};
pub use connection::{Connection, ConnectionStats, PoolKind};
pub use databases::Databases;
pub use error::DatabaseError;
pub use result_handling::{
    NoneHandling, OneHandling, ResultHandling, ResultHandlingError, SeveralHandling, DETAIL_ROWS,
};
pub use scope::{in_evaluation_scope, with_evaluation_scope};
pub use template::{Dialect, Guard, Placeholder, Position, QueryTemplate, SlotKind, TemplateError};
