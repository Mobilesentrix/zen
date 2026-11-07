# 6. Sequential execution mode: items in order, with a state carried between them

## Status

Accepted

## Context

Several PO Plan rules walk ordered rows where row N depends on the result of row N-1: a budget
consumed in warehouse order, an event walk and timeline, an 80-step level bisection, transfer
balancers that hand out stock source by source. `executionMode: "loop"` evaluates every item on its
own, so these rules are written as JavaScript function nodes. A decision table cannot take part.

## Decision

A third value of `executionMode`, `"sequential"`, for every step that has transform attributes:
`decisionNode` (sub-decision), `decisionTableNode` and `expressionNode`. It runs on the same code
path as loop mode (`core/engine/src/nodes/transform_attributes.rs`), so tracing, `passThrough`,
`inputField` and the sub-decision reset between items behave as they do in loop mode.

### JDM

| field | meaning | default |
|---|---|---|
| `executionMode` | `"sequential"` | `"single"` |
| `inputField` | expression giving the array of items, in the order they are evaluated | the node input |
| `stateField` | expression (against the node input) giving the state before the first item | state is `null` |
| `outputPath` | where the array of per-item results is written | none: results are not written |
| `statePath` | where the final state is written | `<outputPath>State` |
| `maxIterations` | most items evaluated; reaching it without a `stop` fails the step | 100000 |
| `passThrough` | each item merged into its result; the node output merged into the node input | `false` |

`outputPath` or `statePath` must be set.

### Per item

- Input: the item's fields plus `state` (`{...item, state}`). An item that is not an object is
  passed as `{item, state}`. A `state` field of the item itself is replaced.
- Output `state`: an object merged into an object state (JSON merge patch: keys not named stay,
  a `null` value removes a key; arrays are replaced); any other value replaces the state. A
  decision table writing only `state.remaining` therefore keeps the rest of the state, and
  constants a step needs (limits, dates) can travel in the state without being copied by each step.
- No `state` in the output: the state is unchanged.
- Output `stop: true`: the step ends after this item (its result is kept).
- The per-item result is the output without `state` and `stop`.
- Items only see their own fields and the state, as in loop mode; anything else from the node input
  is put in the state through `stateField`.

### Output

`{<outputPath>: [results...], <statePath>: finalState}`, then merged into the node input with
`passThrough`. A `null` final state is written as `null`; like any `null` field of a node output, the
graph's merge into the next node's input drops it.

### Errors and trace

- A failing item fails the step. The error text is prefixed `item <index>: ` (0-based), and on a
  traced evaluation the step's `error` entry carries `itemIndex`. A sequential sub-decision inside a
  sequential step names its own item on its own trace entry; the outer entry names the outer item.
- `stateField` errors are reported like `inputField` errors, with `part: "stateField"`.
- A missing `outputPath`/`statePath` and `maxIterations` fail with `part: "config"`.
- `traceData` of the step is an array with one entry per evaluated item, as in loop mode; each
  entry's input shows the state before that item.

### Example: budget consumed in warehouse order

```json
{
  "id": "allocate", "type": "decisionNode", "name": "Allocate budget",
  "content": {
    "key": "consume-one-warehouse",
    "inputField": "warehouses",
    "executionMode": "sequential",
    "stateField": "{remaining: budget}",
    "outputPath": "allocations"
  }
}
```

`consume-one-warehouse` is a decision table with input `need - state.remaining` and outputs
`warehouseId`, `take`, `state.remaining`: `<= 0` → `need`, `state.remaining - need`; otherwise
`state.remaining`, `0`. With `budget: 100` and needs 30, 50.5, 40, 10 the output is
`{"allocations": [{"warehouseId": 1, "take": 30}, {"warehouseId": 2, "take": 50.5},
{"warehouseId": 3, "take": 19.5}, {"warehouseId": 4, "take": 0}], "allocationsState": {"remaining": 0}}`.
Numbers stay decimal throughout.

### Alternatives not taken

- A separate node type: the editor, the graph analysis and the bindings already know transform
  attributes; a mode reuses all of that.
- Replacing the state with the output `state` as is: a decision table writes dotted fields, so it
  would drop every state key it does not write.
- Exposing the node input to each item (`$input`): loop mode does not, and the state carries what a
  step needs.

## Consequences

- The editor analysis (`core/engine/src/workspace/graph/analysis.rs`) types the item input as the
  item plus `state` typed from `stateField`, and the final state from the items' `state` output.
- Per-item cost over loop mode: one copy of the item's field map (to add `state`) and, when the output
  has `state` or `stop`, one copy of the output map. Measured in `core/engine/tests/ms_sequential_bench.rs`
  (release, 1000 items, 3 small fields each): an expression step with the same row costs 2.6 µs/item
  in loop mode and 3.7 µs/item sequential (+1.2 µs, the map copies); with a state update (two
  `min()` rows) 9.5 µs/item; a sub-decision step (decision table) 8.1 µs/item loop and 5.7 µs/item
  sequential, so there the difference is below run-to-run noise.
- Nodes that do not use the mode serialise as before (`stateField`, `statePath` and `maxIterations`
  are omitted when absent).
