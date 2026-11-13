//! The adapter of an engine that has no database connection configured: a Lookup step fails with
//! a message that says so, instead of the engine's generic "custom node handler not provided"
//! or the embedder's own handler receiving a node it does not know.

use crate::adapter::{FallbackAdapter, DATABASE_NODE_KIND};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest};
use zen_engine::nodes::failure::{FailedNode, FailureKind, FailurePart, NodeFailure};
use zen_engine::nodes::{NodeError, NodeResult};

/// The reason a Lookup step fails on an engine without a database configuration. The same text
/// as the Go module's `ErrDatabaseNodeUnavailable`.
pub const DATABASE_UNAVAILABLE: &str = "database node not available: this engine has no database plug-in or no database connection configured";

/// Fails every custom node of kind `"database"`; hands every other kind to the fallback.
pub struct DatabaseUnavailableNode {
    fallback: FallbackAdapter,
}

impl DatabaseUnavailableNode {
    pub fn new(fallback: FallbackAdapter) -> Self {
        Self { fallback }
    }
}

impl fmt::Debug for DatabaseUnavailableNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseUnavailableNode")
            .field("fallback", &self.fallback)
            .finish()
    }
}

impl CustomNodeAdapter for DatabaseUnavailableNode {
    fn handle(&self, request: CustomNodeRequest) -> Pin<Box<dyn Future<Output = NodeResult> + '_>> {
        if request.node.kind.as_ref() != DATABASE_NODE_KIND {
            return self.fallback.handle(request);
        }
        Box::pin(async move {
            let message = format!("node \"{}\": {DATABASE_UNAVAILABLE}", request.node.name);
            let failure = NodeFailure::new(FailureKind::Evaluation, message.clone())
                .with_part(FailurePart::Config);
            Err(NodeError {
                node_id: request.node.id.clone(),
                trace: None,
                source: Box::new(FailedNode::new(failure, message)),
            })
        })
    }
}
