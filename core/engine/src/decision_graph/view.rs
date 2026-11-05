//! One evaluation's view of a decision's graph.
//!
//! Every evaluation shares the decision's prepared graph and records the edges that switch nodes
//! remove (the branches not taken, and recursively nodes left without edges) in two bitsets.
//! Every query skips the deleted edges and nodes, so it answers exactly what the mutated graph
//! answered: `StableGraph` keeps edge lists as linked lists, and removing an edge unlinks it
//! without reordering the rest, so filtering the original lists yields the same sequence.
use crate::decision_graph::walker::StableDiDecisionGraph;
use crate::model::{DecisionEdge, DecisionNode};
use fixedbitset::FixedBitSet;
use petgraph::prelude::{EdgeIndex, NodeIndex};
use petgraph::stable_graph::EdgeReference;
use petgraph::visit::{EdgeIndexable, EdgeRef, NodeIndexable};
use petgraph::{Direction, Incoming, Outgoing};
use std::ops::Index;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct GraphView {
    base: Arc<StableDiDecisionGraph>,
    removed_edges: FixedBitSet,
    removed_nodes: FixedBitSet,
}

impl GraphView {
    pub(crate) fn new(base: Arc<StableDiDecisionGraph>) -> Self {
        Self {
            removed_edges: FixedBitSet::with_capacity(base.edge_bound()),
            removed_nodes: FixedBitSet::with_capacity(base.node_bound()),
            base,
        }
    }

    /// Nothing removed: the view is the shared graph itself.
    pub(crate) fn is_pristine(&self) -> bool {
        self.removed_edges.is_clear() && self.removed_nodes.is_clear()
    }

    pub(crate) fn base(&self) -> &Arc<StableDiDecisionGraph> {
        &self.base
    }

    pub(crate) fn node_bound(&self) -> usize {
        self.base.node_bound()
    }

    #[inline]
    fn has_node(&self, n: NodeIndex) -> bool {
        !self.removed_nodes.contains(n.index())
    }

    #[inline]
    fn has_edge(&self, e: EdgeIndex) -> bool {
        !self.removed_edges.contains(e.index())
    }

    pub(crate) fn node_weight(&self, n: NodeIndex) -> Option<&Arc<DecisionNode>> {
        self.base.node_weight(n).filter(|_| self.has_node(n))
    }

    pub(crate) fn node_indices(&self) -> impl Iterator<Item = NodeIndex> + '_ {
        self.base.node_indices().filter(|n| self.has_node(*n))
    }

    pub(crate) fn edges_directed(
        &self,
        n: NodeIndex,
        direction: Direction,
    ) -> impl Iterator<Item = EdgeReference<'_, Arc<DecisionEdge>>> + '_ {
        self.base
            .edges_directed(n, direction)
            .filter(|e| self.has_edge(e.id()))
    }

    pub(crate) fn neighbors_directed(
        &self,
        n: NodeIndex,
        direction: Direction,
    ) -> impl Iterator<Item = NodeIndex> + '_ {
        self.edges_directed(n, direction)
            .map(move |e| match direction {
                Outgoing => e.target(),
                Incoming => e.source(),
            })
    }

    fn edge_endpoints(&self, e: EdgeIndex) -> Option<(NodeIndex, NodeIndex)> {
        self.base.edge_endpoints(e).filter(|_| self.has_edge(e))
    }

    fn remove_node(&mut self, n: NodeIndex) {
        // StableGraph::remove_node also removes the node's remaining edges.
        let incident: Vec<EdgeIndex> = self
            .edges_directed(n, Outgoing)
            .chain(self.edges_directed(n, Incoming))
            .map(|e| e.id())
            .collect();
        for e in incident {
            self.removed_edges.insert(e.index());
        }
        self.removed_nodes.insert(n.index());
    }

    /// Upstream's `remove_edge_recursive`, on the view: drop the edge; a node left with no
    /// incoming (target side) or no outgoing (source side) edges loses its edges on the other
    /// side too, and is removed when it has no outgoing edges left.
    pub(crate) fn remove_edge_recursive(&mut self, edge_id: EdgeIndex) {
        let Some((source_nid, target_nid)) = self.edge_endpoints(edge_id) else {
            return;
        };

        self.removed_edges.insert(edge_id.index());

        for (nid, direction) in [(target_nid, Incoming), (source_nid, Outgoing)] {
            if self.edges_directed(nid, direction).next().is_none() {
                let edge_ids: Vec<EdgeIndex> = self
                    .edges_directed(nid, direction.opposite())
                    .map(|edge| edge.id())
                    .collect();

                for edge_id in edge_ids {
                    self.remove_edge_recursive(edge_id);
                }

                // `StableGraph::edges` of a directed graph = outgoing edges.
                if self.has_node(nid) && self.edges_directed(nid, Outgoing).next().is_none() {
                    self.remove_node(nid);
                }
            }
        }
    }

    /// The graph with the removals applied (for validating a non-pristine view).
    pub(crate) fn materialize(&self) -> StableDiDecisionGraph {
        let mut graph = (*self.base).clone();
        for e in self.removed_edges.ones() {
            graph.remove_edge(EdgeIndex::new(e));
        }
        for n in self.removed_nodes.ones() {
            graph.remove_node(NodeIndex::new(n));
        }
        graph
    }
}

impl Index<NodeIndex> for GraphView {
    type Output = Arc<DecisionNode>;

    fn index(&self, n: NodeIndex) -> &Self::Output {
        assert!(self.has_node(n), "node {} was removed", n.index());
        &self.base[n]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use petgraph::stable_graph::StableDiGraph;

    fn node(i: usize) -> Arc<DecisionNode> {
        let json = format!(
            r#"{{"id":"n{i}","name":"n{i}","type":"expressionNode","content":{{"expressions":[]}}}}"#
        );
        Arc::new(serde_json::from_str(&json).unwrap())
    }

    fn edge(i: usize) -> Arc<DecisionEdge> {
        Arc::new(DecisionEdge {
            id: format!("e{i}").into(),
            source_id: "".into(),
            target_id: "".into(),
            source_handle: None,
        })
    }

    /// Upstream's mutation of a real graph, the reference the view must match.
    fn remove_edge_recursive(g: &mut StableDiDecisionGraph, edge_id: EdgeIndex) {
        let Some((source_nid, target_nid)) = g.edge_endpoints(edge_id) else {
            return;
        };
        g.remove_edge(edge_id);
        for (nid, direction) in [(target_nid, Incoming), (source_nid, Outgoing)] {
            if g.edges_directed(nid, direction).count() == 0 {
                let edge_ids: Vec<EdgeIndex> = g
                    .edges_directed(nid, direction.opposite())
                    .map(|e| e.id())
                    .collect();
                for edge_id in edge_ids {
                    remove_edge_recursive(g, edge_id);
                }
                if g.edges(nid).count() == 0 {
                    g.remove_node(nid);
                }
            }
        }
    }

    fn snapshot_graph(g: &StableDiDecisionGraph) -> Vec<String> {
        let mut out = vec![];
        for n in g.node_indices() {
            for d in [Outgoing, Incoming] {
                let edges: Vec<usize> = g.edges_directed(n, d).map(|e| e.id().index()).collect();
                let nbs: Vec<usize> = g.neighbors_directed(n, d).map(|x| x.index()).collect();
                out.push(format!("{} {:?} {:?} {:?}", n.index(), d, edges, nbs));
            }
        }
        out
    }

    fn snapshot_view(v: &GraphView) -> Vec<String> {
        let mut out = vec![];
        for n in v.node_indices() {
            for d in [Outgoing, Incoming] {
                let edges: Vec<usize> = v.edges_directed(n, d).map(|e| e.id().index()).collect();
                let nbs: Vec<usize> = v.neighbors_directed(n, d).map(|x| x.index()).collect();
                out.push(format!("{} {:?} {:?} {:?}", n.index(), d, edges, nbs));
            }
        }
        out
    }

    #[test]
    fn view_matches_mutated_graph_on_random_dags() {
        let mut seed: u64 = 7;
        let mut next = move |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        for _ in 0..2000 {
            let nodes = 2 + next(12);
            let mut g: StableDiDecisionGraph = StableDiGraph::new();
            let ids: Vec<NodeIndex> = (0..nodes).map(|i| g.add_node(node(i))).collect();
            let mut e = 0;
            for t in 1..nodes {
                // every node after the first has 1-3 parents among earlier nodes (a DAG)
                for _ in 0..1 + next(3) {
                    let s = next(t);
                    g.add_edge(ids[s], ids[t], edge(e));
                    e += 1;
                }
            }
            let base = Arc::new(g.clone());
            let mut view = GraphView::new(base);
            for _ in 0..1 + next(4) {
                let live: Vec<EdgeIndex> = g.edge_indices().collect();
                if live.is_empty() {
                    break;
                }
                let pick = live[next(live.len())];
                remove_edge_recursive(&mut g, pick);
                view.remove_edge_recursive(pick);
                assert_eq!(snapshot_view(&view), snapshot_graph(&g));
                let ids_g: Vec<usize> = g.node_indices().map(|n| n.index()).collect();
                let ids_v: Vec<usize> = view.node_indices().map(|n| n.index()).collect();
                assert_eq!(ids_v, ids_g);
                assert_eq!(snapshot_graph(&view.materialize()), snapshot_graph(&g));
            }
        }
    }
}
