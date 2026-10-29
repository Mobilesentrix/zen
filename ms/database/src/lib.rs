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
mod decode;
mod pool;
mod scope;
mod template;

pub use adapter::{
    DatabaseCustomNode, ExecutionMode, FallbackAdapter, NoneHandling, OneHandling, ResultHandling,
    ResultHandlingError, SeveralHandling, DATABASE_NODE_KIND, DETAIL_ROWS,
};
pub use bind::{bind, BindError, BindValue, BoundQuery, NodeLabel, MAX_BIND_PARAMS, MAX_COUNT};
pub use config::{
    Backend, ConnectionConfig, DatabaseConfig, DEFAULT_MAX_LOOP_ITEMS, DEFAULT_MAX_ROWS,
};
pub use pool::{Connection, ConnectionStats, DatabaseError, Databases, PoolKind};
pub use scope::{in_evaluation_scope, with_evaluation_scope};
pub use template::{Dialect, Guard, Placeholder, Position, QueryTemplate, SlotKind, TemplateError};
