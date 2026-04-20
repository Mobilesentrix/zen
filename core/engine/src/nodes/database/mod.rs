use crate::nodes::{NodeContext, NodeContextExt, NodeHandler, NodeResult};
use zen_database::DatabaseRequest;
use zen_types::decision::{DatabaseNodeContent, TransformAttributes};
use zen_types::variable::Variable;

#[derive(Debug, Clone)]
pub struct DatabaseNodeHandler;

pub type DatabaseNodeData = DatabaseNodeContent;
pub type DatabaseNodeTrace = Variable;

impl NodeHandler for DatabaseNodeHandler {
    type NodeData = DatabaseNodeData;
    type TraceData = DatabaseNodeTrace;

    fn transform_attributes(
        &self,
        ctx: &NodeContext<Self::NodeData, Self::TraceData>,
    ) -> Option<TransformAttributes> {
        Some(ctx.node.transform_attributes.clone())
    }

    async fn handle(&self, ctx: NodeContext<Self::NodeData, Self::TraceData>) -> NodeResult {
        let query = ctx.node.query.clone();

        let request = DatabaseRequest {
            query: query.to_string(),
        };

        let response = ctx
            .extensions
            .database
            .execute(&request)
            .await
            .node_context_message(&ctx, "Database query execution failed")?;

        let output = serde_json::json!({
            "rows": response.rows,
            "rowsCount": response.row_count,
        });

        ctx.trace(|trace| {
            *trace = output.clone().into();
        });

        ctx.success(output.into())
    }
}
