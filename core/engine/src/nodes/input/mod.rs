pub(crate) mod dates;

use crate::nodes::definition::NodeHandler;
use crate::nodes::result::NodeResult;
use crate::nodes::NodeContext;
use zen_types::decision::InputNodeContent;
use zen_types::variable::Variable;

#[derive(Debug, Clone)]
pub struct InputNodeHandler;

pub type InputNodeData = InputNodeContent;
pub type InputNodeTrace = Variable;

impl NodeHandler for InputNodeHandler {
    type NodeData = InputNodeData;
    type TraceData = InputNodeTrace;

    async fn handle(&self, ctx: NodeContext<Self::NodeData, Self::TraceData>) -> NodeResult {
        if let Some(json_schema) = &ctx.node.schema {
            ctx.validate(json_schema, &ctx.input)?;
        }

        // Without a schema the walk only turns engine date values back into their source
        // text. It visits every object and array of the input, once per rule call; skip it
        // when no date can be in there.
        let schema = ctx.node.schema.as_deref();
        // (Asked first even when a schema exists: the first input node of an evaluation is
        // the one that records whether the evaluation's own input holds a date.)
        let may_hold_dates = ctx.extensions.dates.may_hold(&ctx.input);
        let walk = schema.is_some() || may_hold_dates;
        let output = match walk {
            true => dates::DeclaredDates::prepare(&ctx.input, schema),
            false => None,
        };
        ctx.success(output.unwrap_or_else(|| ctx.input.clone()))
    }
}
