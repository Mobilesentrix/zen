use fixedbitset::FixedBitSet;
use petgraph::matrix_graph::Zero;
use petgraph::prelude::{EdgeIndex, NodeIndex, StableDiGraph};
use petgraph::visit::{EdgeRef, VisitMap};
use petgraph::{Incoming, Outgoing};
use std::ops::Deref;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::config::ZEN_CONFIG;
use crate::decision_graph::view::GraphView;
use crate::model::{
    DecisionEdge, DecisionNode, DecisionNodeKind, SwitchStatement, SwitchStatementHitPolicy,
};
use crate::DecisionGraphTrace;
use zen_expression::variable::{ToVariable, Variable};
use zen_expression::{Isolate, OpcodeCache};

pub(crate) type StableDiDecisionGraph = StableDiGraph<Arc<DecisionNode>, Arc<DecisionEdge>>;

pub(crate) struct NodeData {
    pub name: zen_types::symbol::Symbol,
    pub data: Variable,
    pub nodes_view: Option<Variable>,
}

/// MobileSentrix: node outputs by node index in a dense vector (graph indices are dense and
/// stable). The walker looks up every visited node again after each switch node resets the walk.
#[derive(Default)]
pub(crate) struct NodeDataMap {
    slots: Vec<Option<NodeData>>,
}

impl NodeDataMap {
    #[inline]
    fn get(&self, id: &NodeIndex) -> Option<&NodeData> {
        self.slots.get(id.index())?.as_ref()
    }

    #[inline]
    fn contains_key(&self, id: &NodeIndex) -> bool {
        self.get(id).is_some()
    }

    fn insert(&mut self, id: NodeIndex, value: NodeData) {
        let i = id.index();
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || None);
        }
        self.slots[i] = Some(value);
    }

    fn values(&self) -> impl Iterator<Item = &NodeData> {
        self.slots.iter().filter_map(Option::as_ref)
    }
}

pub(crate) struct GraphWalker {
    iter: usize,
    node_data: NodeDataMap,
    ordered: FixedBitSet,
    to_visit: Vec<NodeIndex>,
    visited_switch_nodes: Vec<NodeIndex>,

    nodes_in_context: bool,
    /// MobileSentrix: nodes that can read `$nodes` (None = all).
    nodes_readers: Option<Arc<ahash::HashSet<Arc<str>>>>,
    /// MobileSentrix: the decision's opcode cache, for switch conditions.
    compiled_cache: Option<Arc<OpcodeCache>>,
}

const ITER_MAX: usize = 1_000;

impl GraphWalker {
    pub fn new(graph: &GraphView) -> Self {
        let mut walker = Self::empty(graph);
        walker.initialize_input_nodes(graph);
        walker
    }

    fn initialize_input_nodes(&mut self, g: &GraphView) {
        // find all initial nodes (nodes without incoming edges)
        self.to_visit.extend(g.node_indices().filter(move |&nid| {
            g.node_weight(nid)
                .is_some_and(|n| matches!(n.kind, DecisionNodeKind::InputNode { content: _ }))
        }));
    }

    fn empty(graph: &GraphView) -> Self {
        Self {
            ordered: FixedBitSet::with_capacity(graph.node_bound()),
            to_visit: Vec::new(),
            node_data: Default::default(),
            visited_switch_nodes: Default::default(),
            iter: 0,

            nodes_in_context: ZEN_CONFIG.nodes_in_context.load(Ordering::Relaxed),
            nodes_readers: None,
            compiled_cache: None,
        }
    }

    pub fn reset(&mut self, g: &GraphView) {
        self.ordered.clear();
        self.to_visit.clear();
        self.initialize_input_nodes(g);

        self.iter += 1;
    }

    pub fn has_node_data(&self, node_id: NodeIndex) -> bool {
        self.node_data.contains_key(&node_id)
    }

    pub fn ending_variables(&self, g: &GraphView) -> Variable {
        g.node_indices()
            .filter(|nid| {
                self.ordered.is_visited(nid)
                    && g.neighbors_directed(*nid, Outgoing).count().is_zero()
            })
            .fold(Variable::empty_object(), |mut acc, curr| {
                match self.node_data.get(&curr) {
                    None => acc,
                    Some(nd) => acc.merge(&nd.data),
                }
            })
    }

    pub fn get_all_node_data(&self) -> Variable {
        let node_values = self
            .node_data
            .values()
            .filter_map(|nd| {
                let value = nd.nodes_view.clone().unwrap_or_else(|| nd.data.clone());
                Some((nd.name.clone(), value))
            })
            .collect();

        Variable::from_object(node_values)
    }

    pub fn set_node_data(&mut self, node_id: NodeIndex, value: NodeData) {
        self.node_data.insert(node_id, value);
    }

    pub fn set_compiled_cache(&mut self, cache: Option<Arc<OpcodeCache>>) {
        self.compiled_cache = cache;
    }

    pub fn set_nodes_readers(&mut self, readers: Option<Arc<ahash::HashSet<Arc<str>>>>) {
        self.nodes_readers = readers;
    }

    /// `$nodes` for `node_id`, built only when that node can read it. A node that never names
    /// `$nodes` behaves the same with or without it.
    pub fn nodes_context_for(&self, node_id: &str) -> Option<Variable> {
        let reads = self
            .nodes_readers
            .as_ref()
            .is_none_or(|readers| readers.contains(node_id));
        (self.nodes_in_context && reads).then(|| self.get_all_node_data())
    }

    pub fn incoming_node_data(&self, g: &GraphView, node_id: NodeIndex) -> (Variable, Variable) {
        let value = self.merge_node_data(g.neighbors_directed(node_id, Incoming));

        (value.clone(), value)
    }

    pub fn merge_node_data<I>(&self, iter: I) -> Variable
    where
        I: Iterator<Item = NodeIndex>,
    {
        let mut incoming = iter.filter_map(|nid| self.node_data.get(&nid));
        let Some(first) = incoming.next() else {
            return Variable::empty_object();
        };

        let head = match &first.data {
            Variable::Object(_) | Variable::Array(_) => first.data.clone(),
            _ => Variable::empty_object(),
        };

        incoming.fold(head, |mut prev, nd| prev.merge_clone(&nd.data))
    }

    /// `graph` is this evaluation's view of the shared graph; switch nodes remove edges in it.
    pub fn next<F: FnMut(DecisionGraphTrace)>(
        &mut self,
        graph: &mut GraphView,
        mut on_trace: Option<F>,
    ) -> Option<NodeIndex> {
        // MobileSentrix: the clock is read only for a trace (upstream reads it on every call).
        let start = on_trace.is_some().then(Instant::now);
        if self.iter >= ITER_MAX {
            return None;
        }
        // Take an unvisited element and find which of its neighbors are next
        while let Some(nid) = self.to_visit.pop() {
            let g: &GraphView = graph;
            if self.ordered.is_visited(&nid) {
                continue;
            }

            if !self.all_dependencies_resolved(g, nid) {
                self.to_visit.push(nid);
                self.to_visit
                    .extend(self.get_unresolved_dependencies(g, nid));
                continue;
            }

            self.ordered.visit(nid);

            // MobileSentrix: only a switch node needs its weight past this point; other nodes
            // (revisited after every switch's reset) do not clone and drop the shared Arc.
            let is_switch = matches!(
                g.node_weight(nid)?.kind,
                DecisionNodeKind::SwitchNode { .. }
            );
            let decision_node = match is_switch {
                true => g.node_weight(nid).cloned(),
                false => None,
            };
            if let Some(DecisionNodeKind::SwitchNode { content }) =
                decision_node.as_deref().map(|node| &node.kind)
            {
                let decision_node = decision_node.as_deref().expect("switch node weight");
                if !self.visited_switch_nodes.contains(&nid) {
                    let (input, input_trace) = self.incoming_node_data(g, nid);
                    let mut isolate =
                        Isolate::with_environment(input).with_cache(self.compiled_cache.clone());
                    if let Some(nodes) = self.nodes_context_for(&decision_node.id) {
                        isolate.set_local(Variable::nodes_key(), nodes);
                    }

                    let mut statement_iter = content.statements.iter();
                    // MobileSentrix: the matching statements by reference; trace rows (which
                    // clone their ids) are built only for a trace.
                    let valid_statements: Vec<&SwitchStatement> = match content.hit_policy {
                        SwitchStatementHitPolicy::First => statement_iter
                            .find(|&s| switch_statement_evaluate(&mut isolate, &s))
                            .into_iter()
                            .collect(),
                        SwitchStatementHitPolicy::Collect => statement_iter
                            .filter(|&s| switch_statement_evaluate(&mut isolate, &s))
                            .collect(),
                    };

                    if let Some(on_trace) = &mut on_trace {
                        let output = input_trace.depth_clone(1);

                        on_trace(DecisionGraphTrace {
                            id: decision_node.id.clone(),
                            name: decision_node.name.clone(),
                            input: input_trace,
                            output,
                            order: 0,
                            performance: Some(Arc::from(format!(
                                "{:.1?}",
                                start.map(|s| s.elapsed()).unwrap_or_default()
                            ))),
                            error: None,
                            trace_data: Some(
                                SwitchStatementTrace {
                                    statements: valid_statements
                                        .iter()
                                        .map(|s| SwitchStatementTraceRow::from((*s).clone()))
                                        .collect(),
                                }
                                .to_variable(),
                            ),
                        });
                    }

                    // Remove all non-valid edges
                    let edges_to_remove: Vec<EdgeIndex> = g
                        .edges_directed(nid, Outgoing)
                        .filter(|edge| {
                            edge.weight().source_handle.as_ref().map_or(true, |handle| {
                                !valid_statements
                                    .iter()
                                    .any(|s| s.id.deref() == handle.deref())
                            })
                        })
                        .map(|edge| edge.id())
                        .collect();
                    let edges_remove_count = edges_to_remove.len();
                    for edge in edges_to_remove {
                        graph.remove_edge_recursive(edge);
                    }

                    self.visited_switch_nodes.push(nid);
                    // Reset the graph if whole branch has been removed
                    if edges_remove_count > 0 {
                        self.reset(graph);
                        continue;
                    }
                }
            }

            let successors = graph.neighbors_directed(nid, Outgoing);
            self.to_visit.extend(successors);

            return Some(nid);
        }

        None
    }

    /// MobileSentrix: the input `incoming_node_data` will give `node_id`, built before the walk
    /// reaches it: an incoming node's output comes from this walk or, for a node run ahead of
    /// it, from `early`. Merged in the same order, so once every incoming node has an output
    /// it is the same value. `None` while an incoming node has no output yet.
    pub fn incoming_node_data_with<'a>(
        &'a self,
        g: &GraphView,
        node_id: NodeIndex,
        early: impl Fn(NodeIndex) -> Option<&'a Variable>,
    ) -> Option<Variable> {
        let mut incoming = Vec::new();
        for nid in g.neighbors_directed(node_id, Incoming) {
            incoming.push(
                self.node_data
                    .get(&nid)
                    .map(|nd| &nd.data)
                    .or_else(|| early(nid))?,
            );
        }
        let mut incoming = incoming.into_iter();
        let Some(first) = incoming.next() else {
            return Some(Variable::empty_object());
        };
        let head = match first {
            Variable::Object(_) | Variable::Array(_) => first.clone(),
            _ => Variable::empty_object(),
        };
        Some(incoming.fold(head, |mut prev, data| prev.merge_clone(data)))
    }

    fn all_dependencies_resolved(&self, g: &GraphView, nid: NodeIndex) -> bool {
        g.neighbors_directed(nid, Incoming)
            .all(|dep| self.ordered.is_visited(&dep))
    }

    fn get_unresolved_dependencies(&self, g: &GraphView, nid: NodeIndex) -> Vec<NodeIndex> {
        g.neighbors_directed(nid, Incoming)
            .filter(|dep| !self.ordered.is_visited(dep))
            .collect()
    }
}

fn switch_statement_evaluate(isolate: &mut Isolate, switch_statement: &SwitchStatement) -> bool {
    if switch_statement.condition.is_empty() {
        return true;
    }

    isolate
        .run_standard(switch_statement.condition.deref())
        .map_or(false, |v| v.as_bool().unwrap_or(false))
}

#[derive(ToVariable)]
struct SwitchStatementTrace {
    statements: Vec<SwitchStatementTraceRow>,
}

#[derive(ToVariable, Clone)]
#[serde(rename_all = "camelCase")]
struct SwitchStatementTraceRow {
    pub id: Arc<str>,
}

impl From<SwitchStatement> for SwitchStatementTraceRow {
    fn from(value: SwitchStatement) -> Self {
        Self { id: value.id }
    }
}
