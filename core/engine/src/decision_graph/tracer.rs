use crate::nodes::failure::{self, NodeFailure};
use crate::nodes::NodeResult;
/// MobileSentrix: traces keep execution order (upstream uses per-instance-seeded hash maps,
/// so a trace serialises its nodes in a different order on every run).
pub type TraceMap<K, V> = indexmap::IndexMap<K, V, ahash::RandomState>;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zen_expression::variable::{ToVariable, VariableMap};
use zen_types::decision::{DecisionNode, DecisionNodeKind};
use zen_types::symbol::Symbol;
use zen_types::variable::Variable;

pub(crate) struct NodeTracer(Option<TraceMap<Arc<str>, DecisionGraphTrace>>);

impl NodeTracer {
    pub fn new(enabled: bool) -> Self {
        Self(enabled.then(TraceMap::default))
    }

    pub fn record_execution(
        &mut self,
        node: &DecisionNode,
        input_trace: Variable,
        result: &NodeResult,
        duration: std::time::Duration,
    ) {
        let Some(traces) = &mut self.0 else {
            return;
        };

        if matches!(node.kind, DecisionNodeKind::SwitchNode { .. }) {
            return;
        }

        let input = match &node.kind {
            DecisionNodeKind::InputNode { .. } => Variable::Null,
            _ => input_trace,
        };

        let mut trace = DecisionGraphTrace {
            id: node.id.clone(),
            name: node.name.clone(),
            input,
            order: traces.len() as u32,
            output: Variable::Null,
            trace_data: None,
            performance: Some(Arc::from(format!("{:.1?}", duration))),
            error: None,
        };

        match &result {
            Ok(ok) => {
                trace.trace_data = ok.trace_data.clone();
                if !matches!(node.kind, DecisionNodeKind::OutputNode { .. }) {
                    trace.output = ok.output.clone();
                }
            }
            Err(err) => {
                trace.trace_data = err.trace.clone();
                trace.error = Some(failure::describe(err.source.as_ref()));
            }
        };

        traces.insert(node.id.clone(), trace);
    }

    pub fn trace_callback(&mut self) -> Option<impl FnMut(DecisionGraphTrace) + '_> {
        let Some(traces) = &mut self.0 else {
            return None;
        };

        Some(|mut trace: DecisionGraphTrace| {
            trace.order = traces.len() as u32;
            traces.insert(trace.id.clone(), trace);
        })
    }

    pub fn into_traces(self) -> Option<TraceMap<Arc<str>, DecisionGraphTrace>> {
        self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionGraphTrace {
    #[serde(default = "null_variable")]
    pub input: Variable,
    #[serde(default = "null_variable")]
    pub output: Variable,
    pub name: Arc<str>,
    pub id: Arc<str>,
    #[serde(default)]
    pub performance: Option<Arc<str>>,
    #[serde(default)]
    pub trace_data: Option<Variable>,
    #[serde(default)]
    pub order: u32,
    /// MobileSentrix: on the node that failed only, what failed and why (absent otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<NodeFailure>,
}

/// Written out (not derived) so a node without `error` keeps upstream's shape exactly; the derive
/// would add `"error": null` to every entry of an error trace and of a sub-decision trace.
impl ToVariable for DecisionGraphTrace {
    fn to_variable(&self) -> Variable {
        let mut map = VariableMap::with_capacity(8);
        map.insert(Symbol::from("input"), self.input.to_variable());
        map.insert(Symbol::from("output"), self.output.to_variable());
        map.insert(Symbol::from("name"), self.name.to_variable());
        map.insert(Symbol::from("id"), self.id.to_variable());
        map.insert(Symbol::from("performance"), self.performance.to_variable());
        map.insert(Symbol::from("traceData"), self.trace_data.to_variable());
        map.insert(Symbol::from("order"), self.order.to_variable());
        if let Some(error) = &self.error {
            map.insert(Symbol::from("error"), error.to_variable());
        }
        Variable::from_object(map)
    }
}

fn null_variable() -> Variable {
    Variable::Null
}
