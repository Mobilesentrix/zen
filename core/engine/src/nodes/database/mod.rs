use crate::nodes::{NodeContext, NodeContextExt, NodeHandler, NodeResult};
use serde_json::Value;
use zen_database::DatabaseRequest;
use zen_tmpl::render;
use zen_types::decision::{DatabaseNodeContent, TransformAttributes};
use zen_types::variable::{ToVariable, Variable};

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

    async fn handle(
            &self,
            ctx: NodeContext<Self::NodeData, Self::TraceData>,
        ) -> NodeResult {
        
        
        todo!()


    }

    
}