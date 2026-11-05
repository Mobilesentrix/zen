# 0003. Evaluation hot path: graph view, node data, isolate

## Status

Accepted.

## Context

Evaluation latency is measured in microseconds, and many evaluations of the same decision run
concurrently. Upstream's per-evaluation work includes mutating a copy of the decision graph when
switch nodes remove branches, looking node outputs up in a map, reading the clock on every call,
and updating an `Arc` counter each time cached bytecode runs.

## Decision

Each change stays small so the fork stays easy to rebase; `git diff zen-engine-v2.1.6` shows them.

- **Graph view** (`core/engine/src/decision_graph/view.rs`). Every evaluation shares the
  decision's prepared graph and records the edges that switch nodes remove (branches not taken,
  and recursively nodes left without edges) in two bitsets. Every query skips the deleted edges
  and nodes. This answers exactly what the mutated graph answered: `StableGraph` keeps edge
  lists as linked lists, and removing an edge unlinks it without reordering the rest, so
  filtering the original lists yields the same sequence.
- **Node data** (`walker.rs`). Node outputs are held in a dense vector indexed by node index,
  because graph indices are dense and stable and the walker looks up every visited node again
  after each switch node resets the walk.
- **Clock** (`walker.rs`). The clock is read only for a trace.
- **Isolate** (`core/expression/src/isolate.rs`). Cached bytecode runs in place through disjoint
  field borrows, so the `Arc`'s shared counter is not updated.

## Consequences

- Concurrent evaluations of one decision share the prepared graph and do not contend on shared
  counters.
- The view must stay behaviourally identical to the mutated graph it replaces, including edge
  order. A petgraph change to `StableGraph` edge-list handling would need re-checking.
- Each change touches an upstream file, so upstream rebases need a manual check at the markers.
