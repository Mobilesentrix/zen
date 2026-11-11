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
        let mut shared = Shared::default();

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
            let key = &*expression.key;
            let reads_earlier = expression.value.contains('$');
            // An expression that reads `$` can return a value that is part of an earlier
            // key's value (`copy = $.x`). The key's value used to be deep-copied on the spot
            // so a later write could not reach both. The copy is now made only when such a
            // write comes (see `Shared`), which keeps what the node returns identical.
            let guarded = shared.guard_needed(&isolate, key);
            if guarded {
                shared.materialize(&ctx, &mut isolate, &result);
            }
            let value = match (reads_earlier, guarded) {
                (true, true) => value.deep_clone(),
                (true, false) => {
                    shared.add(key, &value);
                    value
                }
                (false, _) => {
                    shared.forget(key);
                    value
                }
            };
            ctx.trace(|trace| {
                trace.insert(
                    Rc::from(key),
                    ExpressionNodeTraceItem {
                        result: value.clone(),
                    },
                );
            });

            isolate.insert_dollar(key, value.clone());

            result.dot_insert(key, value);
        }

        ctx.success(result)
    }
}

#[derive(Debug, Clone, ToVariable)]
pub struct ExpressionNodeTraceItem {
    result: Variable,
}

/// Values returned by `$` expressions that have not been deep-copied yet.
///
/// The node returns, for every key, a value that nothing else can write into. A value that
/// is only read again and never written into needs no copy, so the copy is postponed until
/// something could write into a shared object:
///
/// - a key with a dot (`a.b`): `dot_insert` writes into the object stored under `a`, in the
///   result and in `$`, and that object may be part of a pending value (or a pending value
///   may be part of it);
/// - a new key while a pending value holds `$` itself (`copy = $`): `$` gains the key, and
///   so would the pending value.
///
/// Before either, every pending value gets the deep copy it would have had when it was made
/// (nothing has written into anything since), in the result, in `$` and in the trace.
#[derive(Default)]
struct Shared {
    pending: Vec<(Rc<str>, Variable)>,
}

impl Shared {
    fn guard_needed(&self, isolate: &zen_expression::Isolate, key: &str) -> bool {
        key.contains('.') || self.dollar_is_held(isolate)
    }

    /// True when something besides the isolate's own scope holds the object `$`.
    fn dollar_is_held(&self, isolate: &zen_expression::Isolate) -> bool {
        match isolate.scope().local(&Variable::dollar_key()) {
            Some(Variable::Object(dollar)) => Rc::strong_count(dollar) > 1,
            _ => false,
        }
    }

    fn add(&mut self, key: &str, value: &Variable) {
        self.forget(key);
        self.pending.push((Rc::from(key), value.clone()));
    }

    /// The key is written again, so its earlier value is no longer part of the result.
    fn forget(&mut self, key: &str) {
        self.pending.retain(|(pending, _)| &**pending != key);
    }

    fn materialize(
        &mut self,
        ctx: &NodeContext<ExpressionNodeData, ExpressionNodeTrace>,
        isolate: &mut zen_expression::Isolate,
        result: &Variable,
    ) {
        for (key, value) in self.pending.drain(..) {
            let copy = value.deep_clone();
            ctx.trace(|trace| {
                trace.insert(
                    key.clone(),
                    ExpressionNodeTraceItem {
                        result: copy.clone(),
                    },
                );
            });
            isolate.insert_dollar(&key, copy.clone());
            result.dot_insert(&key, copy);
        }
    }
}
