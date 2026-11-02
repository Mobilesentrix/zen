//! Database steps that start before the walk reaches them, so a step's wait on
//! its database overlaps other work of the same evaluation (another step's wait, or nodes the
//! walk runs meanwhile).
//!
//! Everything that depends only on the graph is decided once per rule version, when the decision
//! is compiled ([`EarlyPlan`], kept in the prepared graph): which database steps may start early,
//! which pure nodes feed them, and which node's output can make which of them ready. A rule whose
//! plan is empty (no database step, or no step with work it could overlap) has no per-evaluation
//! state at all. For the others, one evaluation's [`EarlySteps`] is a few words on the stack; it
//! allocates only when a step actually starts.
//!
//! The walk itself is unchanged: every node is still taken in its order, and a node run ahead is
//! recorded when the walk reaches it (trace entry, output, first error), with the result it
//! produced. Its input is checked again then; if a switch removed one of its incoming edges in
//! the meantime, the early result is dropped and the node runs as usual. Work for a node the walk
//! never reaches (an earlier node failed, the output node came first) is dropped; these nodes
//! only read.
use crate::decision_graph::graph::DecisionGraph;
use crate::decision_graph::view::GraphView;
use crate::decision_graph::walker::{GraphWalker, StableDiDecisionGraph};
use crate::model::DecisionNodeKind;
use crate::nodes::NodeResult;
use ahash::HashSet;
use fixedbitset::FixedBitSet;
use futures_util::future::{select, Either};
use futures_util::stream::{FuturesUnordered, StreamExt};
use petgraph::graph::NodeIndex;
use petgraph::visit::NodeIndexable;
use petgraph::{Direction, Incoming, Outgoing};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use zen_expression::variable::Variable;

/// Most nodes (database steps plus the pure nodes feeding them) one plan tracks; per-evaluation
/// state is one bit per candidate. Steps past this limit simply run when the walk reaches them.
const MAX_CANDIDATES: usize = 64;
const NO_SLOT: u8 = u8::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An expression or decision table that never reads `$nodes`: its output depends on its input
    /// only, so it may be computed ahead to feed a database step.
    Pure,
    /// A database step of `GraphContent::concurrent_nodes`.
    Step,
}

/// What may start early in one graph, decided once per compiled rule version.
#[derive(Debug)]
pub(crate) struct EarlyPlan {
    /// Candidates by slot: pure nodes first, then database steps (the order they are tried in).
    nodes: Box<[(NodeIndex, Kind)]>,
    /// By node index: the candidate slots (bits) whose input includes that node's output.
    feeds: Box<[u64]>,
    /// By node index: that node's candidate slot, or `NO_SLOT`.
    slot_of: Box<[u8]>,
}

impl EarlyPlan {
    /// `None` when nothing can start early: no database step may run concurrently
    /// (`concurrent`), or no such step has any node it could overlap, that is a node that is
    /// neither before it nor after it in the graph (a chain gains nothing from starting early).
    pub(crate) fn build(
        graph: &StableDiDecisionGraph,
        concurrent: &HashSet<Arc<str>>,
        readers: &HashSet<Arc<str>>,
    ) -> Option<Self> {
        if concurrent.is_empty() {
            return None;
        }
        let steps: Vec<NodeIndex> = graph
            .node_indices()
            .filter(|&nid| {
                let node = &graph[nid];
                matches!(node.kind, DecisionNodeKind::CustomNode { .. })
                    && concurrent.contains(&node.id)
            })
            .filter(|&nid| has_independent_node(graph, nid))
            .collect();
        if steps.is_empty() {
            return None;
        }

        let is_pure = |nid: NodeIndex| {
            let node = &graph[nid];
            matches!(
                node.kind,
                DecisionNodeKind::ExpressionNode { .. }
                    | DecisionNodeKind::DecisionTableNode { .. }
            ) && !readers.contains(&node.id)
        };
        let mut pure: Vec<NodeIndex> = Vec::new();
        let mut frontier = steps.clone();
        while let Some(nid) = frontier.pop() {
            for dep in graph.neighbors_directed(nid, Incoming) {
                if is_pure(dep) && !pure.contains(&dep) {
                    pure.push(dep);
                    frontier.push(dep);
                }
            }
        }

        let mut nodes: Vec<(NodeIndex, Kind)> = pure.into_iter().map(|n| (n, Kind::Pure)).collect();
        nodes.extend(steps.into_iter().map(|n| (n, Kind::Step)));
        nodes.truncate(MAX_CANDIDATES);

        let bound = graph.node_bound();
        let mut feeds = vec![0u64; bound];
        let mut slot_of = vec![NO_SLOT; bound];
        for (slot, &(nid, _)) in nodes.iter().enumerate() {
            slot_of[nid.index()] = slot as u8;
            for dep in graph.neighbors_directed(nid, Incoming) {
                feeds[dep.index()] |= 1 << slot;
            }
        }
        Some(Self {
            nodes: nodes.into_boxed_slice(),
            feeds: feeds.into_boxed_slice(),
            slot_of: slot_of.into_boxed_slice(),
        })
    }

    #[inline]
    fn feeds(&self, nid: NodeIndex) -> u64 {
        self.feeds.get(nid.index()).copied().unwrap_or(0)
    }

    #[inline]
    fn slot(&self, nid: NodeIndex) -> Option<usize> {
        match self.slot_of.get(nid.index()) {
            Some(&s) if s != NO_SLOT => Some(s as usize),
            _ => None,
        }
    }
}

/// Some node of the graph is neither an ancestor nor a descendant of `step`.
fn has_independent_node(graph: &StableDiDecisionGraph, step: NodeIndex) -> bool {
    let mut related = FixedBitSet::with_capacity(graph.node_bound());
    related.insert(step.index());
    for direction in [Incoming, Outgoing] {
        mark_reachable(graph, step, direction, &mut related);
    }
    graph
        .node_indices()
        .any(|nid| !related.contains(nid.index()))
}

fn mark_reachable(
    graph: &StableDiDecisionGraph,
    from: NodeIndex,
    direction: Direction,
    seen: &mut FixedBitSet,
) {
    let mut stack = vec![from];
    while let Some(nid) = stack.pop() {
        for next in graph.neighbors_directed(nid, direction) {
            if !seen.put(next.index()) {
                stack.push(next);
            }
        }
    }
}

type EarlyStep = Pin<Box<dyn Future<Output = (u8, NodeResult, Duration)>>>;

/// One evaluation's early-started nodes (only for graphs with an [`EarlyPlan`]).
pub(crate) struct EarlySteps {
    plan: Arc<crate::model::PreparedGraph>,
    /// Slots started in this evaluation (each starts at most once).
    started: u64,
    /// Started slots the walk has not taken yet.
    pending: u64,
    /// For a started slot: how many incoming edges its input was built from.
    inputs_at_start: [u8; MAX_CANDIDATES],
    /// Database steps still running (created on the first start).
    running: Option<FuturesUnordered<EarlyStep>>,
    /// Finished early results the walk has not taken yet.
    done: Vec<(u8, NodeResult, Duration)>,
}

impl EarlySteps {
    /// `None` unless the compiled graph has an early-start plan.
    pub(crate) fn new(prepared: Option<&Arc<crate::model::PreparedGraph>>) -> Option<Self> {
        let prepared = prepared?;
        prepared.early.as_ref()?;
        Some(Self {
            plan: prepared.clone(),
            started: 0,
            pending: 0,
            inputs_at_start: [0; MAX_CANDIDATES],
            running: None,
            done: Vec::new(),
        })
    }

    #[inline]
    fn plan(&self) -> &EarlyPlan {
        // `new` checked it is there.
        self.plan.early.as_ref().expect("early plan")
    }

    /// Some started node has not been taken by the walk yet.
    #[inline]
    pub(crate) fn has_pending(&self) -> bool {
        self.pending != 0
    }

    /// Some database step is still running.
    #[inline]
    pub(crate) fn has_running(&self) -> bool {
        self.running.as_ref().is_some_and(|r| !r.is_empty())
    }

    /// The output of `nid` can make some candidate ready.
    #[inline]
    pub(crate) fn feeds_candidate(&self, nid: NodeIndex) -> bool {
        self.plan().feeds(nid) & !self.started != 0
    }

    fn early_output(&self, nid: NodeIndex) -> Option<&Variable> {
        let slot = self.plan().slot(nid)? as u8;
        self.done
            .iter()
            .find(|(s, ..)| *s == slot)
            .and_then(|(_, result, _)| result.as_ref().ok())
            .map(|response| &response.output)
    }

    /// `nid` now has an output: starts every candidate it (and any pure node computed here) makes
    /// ready.
    pub(crate) async fn node_done(
        &mut self,
        graph: &DecisionGraph,
        walker: &GraphWalker,
        nid: NodeIndex,
    ) {
        let mut todo = self.plan().feeds(nid) & !self.started;
        while todo != 0 {
            let slot = todo.trailing_zeros() as usize;
            todo &= todo - 1;
            if let Some(pure) = self.try_start(graph, walker, slot).await {
                todo |= self.plan().feeds(pure) & !self.started;
            }
        }
    }

    /// Starts `slot` if every input is known; returns the node if it was a pure node computed
    /// now (its output may make more candidates ready).
    async fn try_start(
        &mut self,
        graph: &DecisionGraph,
        walker: &GraphWalker,
        slot: usize,
    ) -> Option<NodeIndex> {
        let bit = 1u64 << slot;
        if self.started & bit != 0 {
            return None;
        }
        let (nid, kind) = self.plan().nodes[slot];
        let view = graph.view();
        if walker.has_node_data(nid) {
            return None;
        }
        let node = view.node_weight(nid)?.clone();
        let incoming = view.neighbors_directed(nid, Incoming).count();
        if incoming == 0 || incoming > u8::MAX as usize {
            return None;
        }
        let input = walker.incoming_node_data_with(view, nid, |dep| self.early_output(dep))?;
        self.started |= bit;
        self.pending |= bit;
        self.inputs_at_start[slot] = incoming as u8;
        match kind {
            Kind::Pure => {
                let (result, elapsed) = graph.run_pure(&node, input).await;
                self.done.push((slot as u8, result, elapsed));
                Some(nid)
            }
            Kind::Step => {
                let step =
                    graph.start_step(&node, input, walker.nodes_context_for(&node.id), slot as u8);
                self.running
                    .get_or_insert_with(FuturesUnordered::new)
                    .push(step);
                None
            }
        }
    }

    /// Awaits `fut` while the started steps keep running.
    pub(crate) async fn drive<F: Future>(&mut self, fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        while let Some(running) = self.running.as_mut().filter(|r| !r.is_empty()) {
            match select(fut.as_mut(), running.next()).await {
                Either::Left((output, _)) => return output,
                Either::Right((Some(done), _)) => self.done.push(done),
                Either::Right((None, _)) => break,
            }
        }
        fut.await
    }

    /// The early result of `nid` (waiting for it if it is still running), if it was started and
    /// its input still comes from the same incoming edges.
    pub(crate) async fn take(
        &mut self,
        nid: NodeIndex,
        view: &GraphView,
    ) -> Option<(NodeResult, Duration)> {
        let slot = self.plan().slot(nid)?;
        let bit = 1u64 << slot;
        if self.pending & bit == 0 {
            return None;
        }
        self.pending &= !bit;
        let (result, elapsed) = loop {
            if let Some(i) = self.done.iter().position(|(s, ..)| *s as usize == slot) {
                let (_, result, elapsed) = self.done.swap_remove(i);
                break (result, elapsed);
            }
            let done = self.running.as_mut()?.next().await?;
            self.done.push(done);
        };
        // Switch nodes only ever remove edges, so the same count means the same incoming nodes.
        let incoming = view.neighbors_directed(nid, Incoming).count();
        (incoming == self.inputs_at_start[slot] as usize).then_some((result, elapsed))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::model::{DecisionContent, GraphContent};
    use serde_json::{json, Value};

    fn compiled(nodes: Vec<Value>, edges: Vec<(&str, &str)>) -> GraphContent {
        let edges: Vec<Value> = edges
            .iter()
            .enumerate()
            .map(|(i, (s, t))| json!({ "id": format!("e{i}"), "sourceId": s, "targetId": t, "type": "edge" }))
            .collect();
        let content: DecisionContent =
            serde_json::from_value(json!({ "nodes": nodes, "edges": edges })).unwrap();
        let DecisionContent::Graph(g) = content else {
            panic!("graph")
        };
        let mut g = (*g).clone();
        g.compile();
        g
    }

    fn node(id: &str, kind: &str) -> Value {
        match kind {
            "db" => db(id, "SELECT 1"),
            "write" => db(id, "INSERT INTO log VALUES (1)"),
            "expr" => {
                json!({ "id": id, "name": id, "type": "expressionNode", "content": { "expressions": [] } })
            }
            other => json!({ "id": id, "name": id, "type": other, "content": {} }),
        }
    }

    fn db(id: &str, query: &str) -> Value {
        json!({ "id": id, "name": id, "type": "customNode",
                "content": { "kind": "database", "config": { "query": query } } })
    }

    fn has_plan(g: &GraphContent) -> bool {
        g.prepared_graph.as_ref().is_some_and(|p| p.early.is_some())
    }

    #[test]
    fn no_database_step_means_no_plan() {
        let g = compiled(
            vec![
                node("in", "inputNode"),
                node("a", "expr"),
                node("b", "expr"),
                node("out", "outputNode"),
            ],
            vec![("in", "a"), ("in", "b"), ("a", "out"), ("b", "out")],
        );
        assert!(!has_plan(&g));
    }

    #[test]
    fn a_step_in_a_chain_has_nothing_to_overlap() {
        let g = compiled(
            vec![
                node("in", "inputNode"),
                node("a", "expr"),
                node("q", "db"),
                node("out", "outputNode"),
            ],
            vec![("in", "a"), ("a", "q"), ("q", "out")],
        );
        assert!(!has_plan(&g));
    }

    #[test]
    fn a_step_beside_other_work_is_planned() {
        let g = compiled(
            vec![
                node("in", "inputNode"),
                node("a", "expr"),
                node("q", "db"),
                node("out", "outputNode"),
            ],
            vec![("in", "a"), ("in", "q"), ("a", "out"), ("q", "out")],
        );
        assert!(has_plan(&g));
        let plan = g.prepared_graph.as_ref().unwrap().early.as_ref().unwrap();
        assert_eq!(plan.nodes.len(), 1);
    }

    #[test]
    fn a_step_that_may_write_is_never_planned() {
        let g = compiled(
            vec![
                node("in", "inputNode"),
                node("a", "expr"),
                node("q", "write"),
                node("out", "outputNode"),
            ],
            vec![("in", "a"), ("in", "q"), ("a", "out"), ("q", "out")],
        );
        assert!(!has_plan(&g));
    }
}
