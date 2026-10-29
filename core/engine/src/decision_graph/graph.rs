use crate::decision_graph::cleaner::VariableCleaner;
use crate::decision_graph::early::EarlySteps;
use crate::decision_graph::schema_dict;
use crate::decision_graph::tracer::{NodeTracer, TraceMap};
use crate::decision_graph::view::GraphView;
use crate::decision_graph::walker::{GraphWalker, NodeData, StableDiDecisionGraph};
use crate::engine::EvaluationTraceKind;
use crate::model::{DecisionNodeKind, GraphContent};
use crate::nodes::custom::CustomNodeHandler;
use crate::nodes::decision::DecisionNodeHandler;
use crate::nodes::decision_table::DecisionTableNodeHandler;
use crate::nodes::expression::ExpressionNodeHandler;
use crate::nodes::function::FunctionNodeHandler;
use crate::nodes::input::InputNodeHandler;
use crate::nodes::output::OutputNodeHandler;
use crate::nodes::transform_attributes::TransformAttributesExecution;
use crate::nodes::{
    NodeContext, NodeContextBase, NodeContextConfig, NodeDataType, NodeError, NodeHandler,
    NodeHandlerExtensions, NodeResponse, NodeResult, TraceDataType,
};
use crate::{DecisionGraphTrace, DecisionGraphValidationError, EvaluationError};
use ahash::{HashMap, HashMapExt};
use petgraph::algo::is_cyclic_directed;
use petgraph::matrix_graph::Zero;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::cell::RefCell;
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zen_expression::variable::{ToVariable, Variable};
use zen_types::decision::{DecisionNode, InputNodeContent, OutputNodeContent};

#[derive(Debug)]
pub struct DecisionGraph {
    // MobileSentrix: a compiled decision builds its graph once (GraphContent::compile); an
    // evaluation works on a view of it that records what switch nodes remove.
    initial_graph: Arc<StableDiDecisionGraph>,
    graph: GraphView,
    config: DecisionGraphConfig,
    parent_nodes: Option<Variable>,
    /// MobileSentrix: `config.extensions` shared by this evaluation's node contexts.
    extensions: Rc<NodeHandlerExtensions>,
}

#[derive(Debug)]
pub struct DecisionGraphConfig {
    pub content: Arc<GraphContent>,
    pub trace: bool,
    pub iteration: u8,
    pub max_depth: u8,
    pub extensions: NodeHandlerExtensions,
}

impl DecisionGraph {
    pub fn try_new(config: DecisionGraphConfig) -> Result<Self, DecisionGraphValidationError> {
        let graph = match &config.content.prepared_graph {
            Some(prepared) => prepared.graph.clone()?,
            None => Arc::new(Self::build_graph(config.content.deref())?),
        };
        Ok(Self {
            extensions: Rc::new(config.extensions.clone()),
            graph: GraphView::new(graph.clone()),
            initial_graph: graph,
            config,
            parent_nodes: None,
        })
    }

    pub(crate) fn set_parent_nodes(&mut self, nodes: Option<Variable>) {
        self.parent_nodes = nodes;
    }

    pub(crate) fn build_graph(
        content: &GraphContent,
    ) -> Result<StableDiDecisionGraph, DecisionGraphValidationError> {
        let mut graph = StableDiDecisionGraph::new();
        let mut index_map = HashMap::with_capacity(content.nodes.len());

        for node in &content.nodes {
            let node_id = node.id.clone();
            let node_index = graph.add_node(node.clone());

            index_map.insert(node_id, node_index);
        }

        for edge in &content.edges {
            let source_index = index_map.get(&edge.source_id).ok_or_else(|| {
                DecisionGraphValidationError::MissingNode(edge.source_id.to_string())
            })?;

            let target_index = index_map.get(&edge.target_id).ok_or_else(|| {
                DecisionGraphValidationError::MissingNode(edge.target_id.to_string())
            })?;

            graph.add_edge(*source_index, *target_index, edge.clone());
        }

        Ok(graph)
    }

    pub(crate) fn reset_graph(&mut self) {
        self.graph = GraphView::new(self.initial_graph.clone());
    }

    pub fn validate(&self) -> Result<(), DecisionGraphValidationError> {
        // MobileSentrix: the untouched prepared graph was validated when it was compiled.
        if !self.graph.is_pristine() {
            return Self::validate_graph(&self.graph.materialize());
        }
        if let Some(prepared) = &self.config.content.prepared_graph {
            if prepared
                .graph
                .as_ref()
                .is_ok_and(|g| Arc::ptr_eq(g, self.graph.base()))
            {
                return prepared.validation.clone();
            }
        }
        Self::validate_graph(self.graph.base())
    }

    pub(crate) fn validate_graph(
        graph: &StableDiDecisionGraph,
    ) -> Result<(), DecisionGraphValidationError> {
        let input_count = graph
            .node_weights()
            .filter(|w| matches!(w.kind, DecisionNodeKind::InputNode { .. }))
            .count();
        if input_count != 1 {
            return Err(DecisionGraphValidationError::InvalidInputCount(
                input_count as u32,
            ));
        }

        if is_cyclic_directed(graph) {
            return Err(DecisionGraphValidationError::CyclicGraph);
        }

        Ok(())
    }

    async fn validation_schema(
        &self,
        node_id: &str,
        schema: Option<&serde_json::Value>,
    ) -> Result<Option<(Arc<serde_json::Value>, u64)>, String> {
        let Some(schema) = schema else {
            return Ok(None);
        };
        if let Some(resolved) = &self.config.content.resolved_schemas {
            return Ok(resolved.get(node_id).cloned());
        }
        if !schema_dict::schema_references_dictionary(schema) {
            return Ok(None);
        }

        let dictionaries = schema_dict::load_import_dictionaries(
            self.config.extensions.loader(),
            &self.config.content.imports,
        )
        .await?;
        schema_dict::resolve_schema(schema, &dictionaries)
            .map(|resolved| Some((Arc::new(resolved.0), resolved.1)))
    }

    fn build_node_context(
        &self,
        node: &DecisionNode,
        input: Variable,
        nodes: Option<Variable>,
    ) -> NodeContextBase {
        NodeContextBase {
            id: node.id.clone(),
            name: node.name.clone(),
            input,
            nodes,
            extensions: self.extensions.clone(),
            iteration: self.config.iteration,
            trace: match self.config.trace {
                true => Some(RefCell::new(Variable::Null)),
                false => None,
            },
            config: NodeContextConfig {
                max_depth: self.config.max_depth,
                trace: self.config.trace,
                ..Default::default()
            },
        }
    }

    /// This evaluation's view of the graph.
    pub(crate) fn view(&self) -> &GraphView {
        &self.graph
    }

    /// Computes a pure node (expression or decision table) ahead of the walk (see
    /// `decision_graph::early`).
    pub(crate) async fn run_pure(
        &self,
        node: &DecisionNode,
        input: Variable,
    ) -> (NodeResult, Duration) {
        let base_ctx = self.build_node_context(node, input, None);
        let start = self.config.trace.then(Instant::now);
        let result = match &node.kind {
            DecisionNodeKind::ExpressionNode { content } => {
                handle_node(base_ctx, content.clone(), ExpressionNodeHandler).await
            }
            DecisionNodeKind::DecisionTableNode { content } => {
                handle_node(base_ctx, content.clone(), DecisionTableNodeHandler).await
            }
            _ => unreachable!("only expression and decision table nodes are pure"),
        };
        (result, start.map(|s| s.elapsed()).unwrap_or_default())
    }

    /// The future of a database step started ahead of the walk (see `decision_graph::early`).
    pub(crate) fn start_step(
        &self,
        node: &DecisionNode,
        input: Variable,
        nodes: Option<Variable>,
        slot: u8,
    ) -> Pin<Box<dyn Future<Output = (u8, NodeResult, Duration)>>> {
        let DecisionNodeKind::CustomNode { content } = &node.kind else {
            unreachable!("early steps are custom nodes");
        };
        let base_ctx = self.build_node_context(node, input, nodes);
        let content = content.clone();
        let trace = self.config.trace;
        Box::pin(async move {
            let start = trace.then(Instant::now);
            let result = handle_node(base_ctx, content, CustomNodeHandler).await;
            (slot, result, start.map(|s| s.elapsed()).unwrap_or_default())
        })
    }

    /// Runs one node of the walk (the walk's own dispatch by node kind).
    async fn run_node(
        &self,
        node: &DecisionNode,
        mut base_ctx: NodeContextBase,
        context: &Variable,
        input_trace: &Variable,
    ) -> NodeResult {
        match &node.kind {
            DecisionNodeKind::InputNode { content } => {
                base_ctx.input = context.clone();
                match self
                    .validation_schema(&node.id, content.schema.as_deref())
                    .await
                {
                    Err(message) => base_ctx.error(message),
                    Ok(None) => handle_node(base_ctx, content.clone(), InputNodeHandler).await,
                    Ok(Some((schema, salt))) => {
                        base_ctx.config.validation_salt = salt;
                        let resolved = InputNodeContent {
                            schema: Some(schema),
                        };
                        handle_node(base_ctx, resolved, InputNodeHandler).await
                    }
                }
            }
            DecisionNodeKind::OutputNode { content } => {
                match self
                    .validation_schema(&node.id, content.schema.as_deref())
                    .await
                {
                    Err(message) => base_ctx.error(message),
                    Ok(None) => handle_node(base_ctx, content.clone(), OutputNodeHandler).await,
                    Ok(Some((schema, salt))) => {
                        base_ctx.config.validation_salt = salt;
                        let resolved = OutputNodeContent {
                            schema: Some(schema),
                        };
                        handle_node(base_ctx, resolved, OutputNodeHandler).await
                    }
                }
            }
            DecisionNodeKind::SwitchNode { .. } => Ok(NodeResponse {
                output: input_trace.clone(),
                trace_data: None,
            }),
            DecisionNodeKind::FunctionNode { content } => {
                handle_node(base_ctx, content.clone(), FunctionNodeHandler).await
            }
            DecisionNodeKind::DecisionNode { content } => {
                handle_node(base_ctx, content.clone(), DecisionNodeHandler::default()).await
            }
            DecisionNodeKind::DecisionTableNode { content } => {
                handle_node(base_ctx, content.clone(), DecisionTableNodeHandler).await
            }
            DecisionNodeKind::ExpressionNode { content } => {
                handle_node(base_ctx, content.clone(), ExpressionNodeHandler).await
            }
            DecisionNodeKind::CustomNode { content } => {
                handle_node(base_ctx, content.clone(), CustomNodeHandler).await
            }
        }
    }

    pub async fn evaluate(
        &mut self,
        context: Variable,
    ) -> Result<DecisionGraphResponse, Box<EvaluationError>> {
        let root_start = Instant::now();

        self.validate()?;
        if self.config.iteration >= self.config.max_depth {
            return Err(Box::new(EvaluationError::DepthLimitExceeded));
        }

        let mut walker = GraphWalker::new(&self.graph);
        walker.set_nodes_readers(self.config.content.nodes_readers.clone());
        walker.set_compiled_cache(self.extensions.compiled_cache.clone());
        let mut tracer = NodeTracer::new(self.config.trace);
        // MobileSentrix: database steps start as soon as their inputs are known (see
        // `decision_graph::early`); the walk below still takes every node in its own order. A
        // graph whose compiled plan has nothing to start early has no state here.
        let mut early = EarlySteps::new(self.config.content.prepared_graph.as_ref());

        while let Some(nid) = walker.next(&mut self.graph, tracer.trace_callback()) {
            if walker.has_node_data(nid) {
                continue;
            }

            let node = &self.graph[nid];
            let start = self.config.trace.then(Instant::now);
            let (input, input_trace) = walker.incoming_node_data(&self.graph, nid);
            let base_ctx =
                self.build_node_context(node.deref(), input, walker.nodes_context_for(&node.id));

            let taken = match early.as_mut() {
                Some(early) if early.has_pending() => early.take(nid, &self.graph).await,
                _ => None,
            };
            let (node_execution, elapsed) = match taken {
                Some(done) => done,
                None => {
                    let run = self.run_node(node, base_ctx, &context, &input_trace);
                    // While started steps run, the node is awaited beside them (boxed: that
                    // state only exists for evaluations with a step in flight).
                    let execution = match early.as_mut() {
                        Some(early) if early.has_running() => Box::pin(early.drive(run)).await,
                        _ => run.await,
                    };
                    (execution, start.map(|s| s.elapsed()).unwrap_or_default())
                }
            };

            tracer.record_execution(node.deref(), input_trace, &node_execution, elapsed);

            let output = match node_execution {
                Ok(ok) => ok.output,
                Err(err) => return Err(node_failure(tracer, err)),
            };

            let nodes_view = match (&node.kind, &self.parent_nodes) {
                (DecisionNodeKind::InputNode { .. }, Some(parent_nodes)) => {
                    let view = output.depth_clone(1);
                    view.dot_insert(Variable::nodes_key().as_ref(), parent_nodes.clone());
                    Some(view)
                }
                _ => None,
            };

            walker.set_node_data(
                nid,
                NodeData {
                    name: zen_types::symbol::Symbol::from(node.name.deref()),
                    data: output,
                    nodes_view,
                },
            );

            // Terminate once Output node is reached
            if matches!(node.kind, DecisionNodeKind::OutputNode { .. }) {
                break;
            }
            if let Some(early) = early.as_mut().filter(|e| e.feeds_candidate(nid)) {
                Box::pin(early.node_done(self, &walker, nid)).await;
            }
        }

        let result = walker.ending_variables(&self.graph);
        let trace = tracer.into_traces();

        if self.config.iteration.is_zero() {
            let mut cleaner = VariableCleaner::new();
            cleaner.clean(&result);
            if let Some(t) = &trace {
                t.values().for_each(|v| {
                    cleaner.clean(&v.input);
                    cleaner.clean(&v.output);
                    if let Some(td) = &v.trace_data {
                        cleaner.clean(td);
                    }
                })
            }
        }

        Ok(DecisionGraphResponse {
            performance: format!("{:.1?}", root_start.elapsed()),
            result,
            trace: trace.map(EvaluationTrace::Graph),
        })
    }
}

/// The evaluation error for a failed node, with the trace of the nodes run so far.
fn node_failure(tracer: NodeTracer, err: NodeError) -> Box<EvaluationError> {
    let trace = tracer.into_traces();
    if let Some(t) = &trace {
        let mut cleaner = VariableCleaner::new();
        t.values().for_each(|v| {
            cleaner.clean(&v.input);
            cleaner.clean(&v.output);
            if let Some(td) = &v.trace_data {
                cleaner.clean(td);
            }
        })
    }

    Box::new(EvaluationError::NodeError {
        node_id: err.node_id,
        source: err.source,
        trace: trace.map(|t| t.to_variable()),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum EvaluationTrace {
    Graph(TraceMap<Arc<str>, DecisionGraphTrace>),
    Policy(crate::policy::Trace),
}

impl EvaluationTrace {
    pub fn as_graph(&self) -> Option<&TraceMap<Arc<str>, DecisionGraphTrace>> {
        match self {
            Self::Graph(m) => Some(m),
            Self::Policy(_) => None,
        }
    }

    pub fn into_graph(self) -> Option<TraceMap<Arc<str>, DecisionGraphTrace>> {
        match self {
            Self::Graph(m) => Some(m),
            Self::Policy(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionGraphResponse {
    pub performance: String,
    pub result: Variable,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<EvaluationTrace>,
}

impl DecisionGraphResponse {
    pub fn serialize_with_mode<S>(
        &self,
        serializer: S,
        mode: EvaluationTraceKind,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("performance", &self.performance)?;
        map.serialize_entry("result", &self.result)?;
        if let Some(trace) = &self.trace {
            match trace {
                EvaluationTrace::Graph(graph_trace) => {
                    map.serialize_entry(
                        "trace",
                        &mode.serialize_trace(&graph_trace.to_variable()),
                    )?;
                }
                EvaluationTrace::Policy(policy_trace) => match mode {
                    EvaluationTraceKind::String | EvaluationTraceKind::ReferenceString => {
                        map.serialize_entry(
                            "trace",
                            &serde_json::to_string(policy_trace).unwrap_or_default(),
                        )?;
                    }
                    _ => {
                        map.serialize_entry("trace", policy_trace)?;
                    }
                },
            }
        }

        map.end()
    }
}

async fn handle_node<NodeData, TraceData, NodeHandlerType>(
    base_ctx: NodeContextBase,
    content: NodeData,
    handler: NodeHandlerType,
) -> NodeResult
where
    TraceData: TraceDataType,
    NodeData: NodeDataType,
    NodeHandlerType: NodeHandler<NodeData = NodeData, TraceData = TraceData>,
{
    let ctx = NodeContext::<NodeData, TraceData>::from_base(base_ctx.clone(), content);
    if let Some(transform_attributes) = handler.transform_attributes(&ctx) {
        return transform_attributes
            .run_with(base_ctx, move |input, has_more| {
                let handler = handler.clone();
                let mut new_ctx = ctx.clone();
                new_ctx.input = input;

                async move {
                    match has_more {
                        false => handler.handle(new_ctx).await,
                        true => {
                            let result = handler.handle(new_ctx.clone()).await;
                            handler.after_transform_attributes(&new_ctx).await?;
                            result
                        }
                    }
                }
            })
            .await;
    }

    handler.handle(ctx).await
}
