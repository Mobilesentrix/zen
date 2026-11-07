use crate::model::{TransformAttributes, TransformExecutionMode};
use crate::nodes::failure::{self, FailedNode, FailureKind, FailurePart, NodeFailure};
use crate::nodes::result::{NodeError, NodeResult};
use crate::nodes::NodeContextBase;
use std::future::Future;
use std::ops::Deref;
use zen_expression::Variable;
use zen_types::decision::SEQUENTIAL_MAX_ITERATIONS;

/// Sequential mode: the key of an item's input and output that carries the state.
const STATE_KEY: &str = "state";
/// Sequential mode: the key of an item's output that ends the step after that item.
const STOP_KEY: &str = "stop";
/// Sequential mode: the key a non-object item is passed under.
const ITEM_KEY: &str = "item";

pub(crate) trait TransformAttributesExecution {
    async fn run_with<F, Fut>(&self, ctx: NodeContextBase, evaluate: F) -> NodeResult
    where
        F: Fn(Variable, bool) -> Fut,
        Fut: Future<Output = NodeResult>;
}

impl TransformAttributesExecution for TransformAttributes {
    async fn run_with<F, Fut>(&self, ctx: NodeContextBase, evaluate: F) -> NodeResult
    where
        F: Fn(Variable, bool) -> Fut,
        Fut: Future<Output = NodeResult>,
    {
        let input = match &self.input_field {
            None => ctx.input.clone(),
            Some(input_field) => evaluate_field(&ctx, input_field, FailurePart::InputField)?,
        };

        // let mut trace_data: Option<Variable> = None;
        let mut output = match self.execution_mode {
            TransformExecutionMode::Single => {
                let response = evaluate(input, false).await?;
                if let Some(td) = response.trace_data {
                    ctx.trace(|t| {
                        *t = td;
                    });
                }

                response.output
            }
            TransformExecutionMode::Loop => {
                let Some(input_array_ref) = input.as_array() else {
                    return expected_array(&ctx, self, &input);
                };
                let input_array = input_array_ref.borrow();
                ctx.trace(|t| {
                    *t = Variable::from_array(Vec::with_capacity(input_array.len()));
                });

                let mut output_array = Vec::with_capacity(input_array.len());
                for (index, input) in input_array.iter().enumerate() {
                    let has_more = index < input_array.len() - 1;
                    let mut response = evaluate(input.clone(), has_more).await?;
                    if let Some(td) = response.trace_data {
                        push_trace(&ctx, td);
                    }

                    if self.pass_through {
                        response.output = input.clone().merge_clone(&response.output);
                    }

                    output_array.push(response.output);
                }

                Variable::from_array(output_array)
            }
            TransformExecutionMode::Sequential => {
                return run_sequential(self, ctx, input, evaluate).await;
            }
        };

        if let Some(output_path) = &self.output_path {
            let new_output = Variable::empty_object();
            new_output.dot_insert(output_path.deref(), output);

            output = new_output;
        }

        if self.pass_through {
            let mut node_input = ctx.input.clone();
            output = node_input.merge_clone(&output)
        }

        ctx.success(output)
    }
}

/// `executionMode: "sequential"`: the items of `input` one at a time, in order. Item `i` is
/// evaluated with `{...item, state}` (`{item, state}` when the item is not an object), where
/// `state` is the state after item `i - 1` (the `stateField` value before the first item).
/// An item's output `state` updates it (see [`next_state`]); an output without `state` leaves
/// it unchanged; `stop: true` ends the step after that item. The node output holds the per-item results (without `state` and `stop`)
/// at `outputPath` and the final state at `statePath` (default `<outputPath>State`).
async fn run_sequential<F, Fut>(
    attributes: &TransformAttributes,
    ctx: NodeContextBase,
    input: Variable,
    evaluate: F,
) -> NodeResult
where
    F: Fn(Variable, bool) -> Fut,
    Fut: Future<Output = NodeResult>,
{
    let Some(state_path) = attributes.final_state_path() else {
        let message = "sequential execution needs an outputPath or a statePath";
        if !ctx.config.trace {
            return ctx.error(message.to_string());
        }
        let failure = NodeFailure::new(FailureKind::Parse, message).with_part(FailurePart::Config);
        return ctx.error(FailedNode::new(failure, message.to_string()));
    };
    let Some(input_array_ref) = input.as_array() else {
        return expected_array(&ctx, attributes, &input);
    };
    let mut state = match &attributes.state_field {
        None => Variable::Null,
        Some(field) => evaluate_field(&ctx, field, FailurePart::StateField)?,
    };
    let max_iterations = usize::try_from(
        attributes
            .max_iterations
            .unwrap_or(SEQUENTIAL_MAX_ITERATIONS),
    )
    .unwrap_or(usize::MAX);

    // the items (shared, not copied), so no borrow of the input array is held across an item
    let input_array: Vec<Variable> = input_array_ref
        .borrow()
        .iter()
        .map(Variable::shallow_clone)
        .collect();
    ctx.trace(|t| {
        *t = Variable::from_array(Vec::with_capacity(input_array.len()));
    });

    let mut results = Vec::with_capacity(input_array.len());
    for (index, item) in input_array.iter().enumerate() {
        if index >= max_iterations {
            return iterations_exceeded(&ctx, max_iterations);
        }
        let has_more = index < input_array.len() - 1;
        let response = evaluate(item_input(item, state.clone()), has_more)
            .await
            .map_err(|error| item_error(error, index))?;
        if let Some(td) = response.trace_data {
            push_trace(&ctx, td);
        }

        let step = Step::from_output(response.output);
        if let Some(next) = step.state {
            state = next_state(state, next);
        }
        let result = match attributes.pass_through {
            true => item.clone().merge_clone(&step.result),
            false => step.result,
        };
        results.push(result);
        if step.stop {
            break;
        }
    }

    let output = Variable::empty_object();
    if let Some(output_path) = &attributes.output_path {
        output.dot_insert(output_path.deref(), Variable::from_array(results));
    }
    output.dot_insert(state_path.deref(), state);

    let output = match attributes.pass_through {
        true => ctx.input.clone().merge_clone(&output),
        false => output,
    };
    ctx.success(output)
}

/// One item's output split into its result, the next state and the stop flag.
struct Step {
    result: Variable,
    state: Option<Variable>,
    stop: bool,
}

impl Step {
    fn from_output(output: Variable) -> Self {
        let Variable::Object(object) = &output else {
            return Self {
                result: output,
                state: None,
                stop: false,
            };
        };
        let source = object.borrow();
        let stop = source
            .get_str(STOP_KEY)
            .and_then(Variable::as_bool)
            .unwrap_or(false);
        let state = source.get_str(STATE_KEY).map(Variable::shallow_clone);
        if state.is_none() && !source.contains_key_str(STOP_KEY) {
            drop(source);
            return Self {
                result: output,
                state,
                stop,
            };
        }
        // a new map: the output may share its map with the sub-decision's own data
        let mut map = source.clone();
        map.remove_str(STATE_KEY);
        map.remove_str(STOP_KEY);
        Self {
            result: Variable::from_object(map),
            state,
            stop,
        }
    }
}

/// The state after an item: an object `state` output is merged into an object state (JSON merge
/// patch: keys it does not name are kept, a `null` value removes a key), so a decision table
/// writing `state.remaining` keeps the rest of the state; any other value replaces the state.
fn next_state(mut state: Variable, output: Variable) -> Variable {
    match (&state, &output) {
        (Variable::Object(_), Variable::Object(_)) => state.merge_clone(&output),
        _ => output,
    }
}

/// The input of one sequential item: its fields and `state`, or `{item, state}`.
fn item_input(item: &Variable, state: Variable) -> Variable {
    let mut map = match item {
        Variable::Object(object) => object.borrow().clone(),
        other => {
            let mut map = zen_types::variable::VariableMap::with_capacity(2);
            map.insert(Variable::key(ITEM_KEY), other.shallow_clone());
            map
        }
    };
    map.insert(Variable::key(STATE_KEY), state);
    Variable::from_object(map)
}

/// The error of item `index`, naming the item in its message and trace entry.
fn item_error(error: NodeError, index: usize) -> NodeError {
    NodeError {
        node_id: error.node_id,
        trace: error.trace,
        source: Box::new(failure::item(index, error.source)),
    }
}

fn iterations_exceeded(ctx: &NodeContextBase, max_iterations: usize) -> NodeResult {
    let message = format!(
        "sequential execution reached maxIterations ({max_iterations}) before an item returned stop"
    );
    if !ctx.config.trace {
        return ctx.error(message);
    }
    let failure =
        NodeFailure::new(FailureKind::Evaluation, message.clone()).with_part(FailurePart::Config);
    ctx.error(FailedNode::new(failure, message))
}

fn push_trace(ctx: &NodeContextBase, trace: Variable) {
    ctx.trace(|var| {
        if let Variable::Array(arr) = var {
            arr.borrow_mut().push(trace);
        };
    });
}

/// Evaluates `field` (the input field or the state field) against the node input.
fn evaluate_field(
    ctx: &NodeContextBase,
    field: &str,
    part: FailurePart,
) -> Result<Variable, NodeError> {
    let mut isolate = ctx.isolate();
    isolate.run_standard(field).map_err(|error| {
        let message = format!("Failed to evaluate expression: {error}");
        if !ctx.config.trace {
            return ctx.make_error(message);
        }
        // Traced, the trace entry names the failing field.
        let env = ctx.input_with_nodes();
        let failure = NodeFailure::expression(part, field, &error, &env, None);
        ctx.make_error(FailedNode::new(failure, message))
    })
}

fn expected_array(
    ctx: &NodeContextBase,
    attributes: &TransformAttributes,
    input: &Variable,
) -> NodeResult {
    if !ctx.config.trace {
        return ctx.error("Expected an array".to_string());
    }
    let failure = NodeFailure::new(
        FailureKind::Type,
        format!("Expected an array, got {}", input.type_name()),
    )
    .with_part(FailurePart::InputField);
    let failure = match &attributes.input_field {
        Some(field) => failure.with_expression(field.deref()),
        None => failure,
    };
    ctx.error(FailedNode::new(failure, "Expected an array".to_string()))
}
