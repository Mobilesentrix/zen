use crate::model::ExpressionNodeContent;
use crate::nodes::result::NodeResult;
use std::rc::Rc;

use crate::nodes::context::NodeContext;
use crate::nodes::definition::NodeHandler;
use crate::nodes::failure::{FailedNode, FailurePart, NodeFailure};
use zen_expression::variable::{ToVariable, Variable};
use zen_types::decision::TransformAttributes;

#[derive(Debug, Clone)]
pub struct ExpressionNodeHandler;

pub type ExpressionNodeData = ExpressionNodeContent;
pub type ExpressionNodeTrace = crate::decision_graph::TraceMap<Rc<str>, ExpressionNodeTraceItem>;

impl NodeHandler for ExpressionNodeHandler {
    type NodeData = ExpressionNodeData;
    type TraceData = ExpressionNodeTrace;

    fn transform_attributes(
        &self,
        ctx: &NodeContext<Self::NodeData, Self::TraceData>,
    ) -> Option<TransformAttributes> {
        Some(ctx.node.transform_attributes.clone())
    }

    async fn handle(&self, ctx: NodeContext<Self::NodeData, Self::TraceData>) -> NodeResult {
        let result = Variable::empty_object();
        let mut isolate = ctx.isolate();

        for expression in ctx.node.expressions.iter() {
            if expression.key.is_empty() || expression.value.is_empty() {
                continue;
            }

            let value = match isolate.run_standard(&expression.value) {
                Ok(value) => value,
                Err(error) => {
                    let message =
                        format!(r#"Failed to evaluate expression: "{}""#, expression.value);
                    if !ctx.config.trace {
                        return ctx.error(message);
                    }
                    // Traced, the failing node's trace entry says what failed.
                    let failure = NodeFailure::expression(
                        FailurePart::Expression,
                        &expression.value,
                        &error,
                        &ctx.input_with_nodes(),
                        Some(&result),
                    )
                    .with_key(&*expression.key);
                    return ctx.error(FailedNode::new(failure, message));
                }
            };
            let value = match expression.value.contains('$') {
                true => value.deep_clone(),
                false => value,
            };
            ctx.trace(|trace| {
                trace.insert(
                    Rc::from(&*expression.key),
                    ExpressionNodeTraceItem {
                        result: value.clone(),
                    },
                );
            });

            isolate.insert_dollar(&expression.key, value.clone());

            result.dot_insert(&expression.key, value);
        }

        ctx.success(result)
    }
}

#[derive(Debug, Clone, ToVariable)]
pub struct ExpressionNodeTraceItem {
    result: Variable,
}
