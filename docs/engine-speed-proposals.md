# Engine speed changes P1, P2 and P3 (all shipped)

Measured on the PO Plan Daily rules (7 decide rules, all without JavaScript) while looking for where the
engine spends time per SKU. Three engine changes were prototyped on a copy of this fork at 794be1f:

- **P1, regex cache for `extract()` / `matches()`: shipped** (CHANGELOG, 2026-11-08 release).
- **P2 and P3: shipped in the durable forms below** (2026-11-11). The original proposals are kept after
  the shipped notes, because the "behaviour risk" paragraphs say what the tests have to pin.

## Shipped: P2 and P3 in their durable forms

### P2, input-node date walk only when an engine date can exist

Upstream's input node calls `dates::DeclaredDates::prepare` on every rule call, a recursive walk of the whole
input that, without an input schema, only turns engine date values back into their source text. The walk
is now skipped for a node without an input schema when no engine date value can be in the input:

- engine dates are created in one place (`DateValue::wrap`, used by the date functions of expressions and
  by `DateValue::from_text` for schema-declared dates), which counts them per thread (`DateValue::created`);
- each evaluation has a `DateWatch` (in the node extensions, shared by sub-rule calls). The first input node
  of the evaluation checks the evaluation's own input once (a read-only scan, no allocation) and records
  whether it holds a date; later input nodes walk when it did, when a schema is present, or when the
  counter moved since the evaluation started (a date was made, by this evaluation or, harmlessly, by another
  one on the same thread).

Inputs parsed from JSON (the C API, HTTP and Java paths) hold no dates, so for them the walk never runs unless
a schema declares dates. A sub-rule call from a parent that produced dates still walks, and sees the source
text exactly as before. Evaluations are not `Send` (they hold `Rc`), so a thread-local counter cannot miss a
date made by the evaluation itself.

Tests (`core/engine/tests/ms_engine_speed.rs`, `nodes::extensions::tests`): a date declared by the root
input schema reaches a sub-rule without a schema as text (`type()` is `string`), also nested in an array of
objects. Red check: with the skip forced on, both fail (`type()` is `date`); on 615aa9b both pass.

### P3, copy-on-write for expression results that read `$`

An expression whose text contains `$` used to deep-copy its result on the spot, so a later key's
`dot_insert` could not write into a value another key still references. The copy is now made only when
such a write comes (`Shared` in `core/engine/src/nodes/expression/mod.rs`):

- a key with a dot (`x.y`) writes into the object stored under `x`, in the result and in `$`;
- a new key while a pending value holds `$` itself (`all = $`) would add the key to that value too.

Before either, every pending value gets the deep copy it would have had when it was made (nothing has
written into anything since), in the result, in `$` and in the trace, so what the node returns, and its trace,
is the same as before. A key written twice drops the first value's pending copy. `ending_variables` now
merges copy-on-write (`merge_clone`, as the other merges of node data already do), so the only in-place
merge of node data cannot write into a value two keys of one node now share.

Tests: aliasing cases per key shape (write to the source, write to the copy, two copies, nested part, object
or array around a part, the whole `$`, `$` inside an object, key written twice, key rewritten then written
into, trace follows the result), and 260 generated key sequences (traced and not) whose answers are compared
line by line with the answers of 615aa9b (`core/engine/tests/data/ms_expression_alias_golden.txt`). Red check:
with the guard forced off, aliasing cases fail and the test run aborts.
`untouched_copies_share_the_value_of_their_source` is the one test that is red on 615aa9b: it pins the gain.

### Parity and speed

All 1,147 saved request cases of the 7 PO Plan rules give identical results on 615aa9b and on this
change, untraced, and a traced sample of 40 per rule is identical too (ship-mode 149, safe-stock 70,
transfer-candidates 153, transfers 265, delegated-buying 103, moq 264, purchase-and-cancel 143).

One thread, microseconds per SKU, median of 3 calls over all normal-path saved cases, 250 SKUs per call (rule
6 at 60 SKUs per call), release build, two rounds per column (615aa9b already has P1):

| Rule | 615aa9b | P2 + P3 |
|---|---|---|
| 1 ship-mode | 299 / 312 | 276 / 298 |
| 2 safe-stock | 244 / 247 | 245 / 247 |
| 3 transfer-source-options | 103 / 100 | 108 / 104 |
| 4 warehouse-transfers | 1,438 / 1,325 | 1,161 / 1,132 |
| 5 delegated-buying | 523 / 530 | 499 / 492 |
| 6 moq-extra-quantity | 6,508 / 6,275 | **978 / 948** |
| 7 purchase-and-cancel | 648 / 650 | 627 / 620 |

Rule 6 is 6.6 times faster, rule 4 about 17 percent. The other rules are within noise to 5 percent. The saved
cases carry small run-wide tables; on real runs, where every item carries the route, lane and ship-time
tables, both costs grow with the input.

# The original proposals (kept for the reasoning)

With P1, P2 and P3 all switched on, all 1,147 saved request cases gave output identical to 794be1f.

## Measurements

Local engine (zen 794be1f evaluator), one thread, microseconds per SKU over all normal-path saved cases,
250 SKUs per call (rule 6 at 60 SKUs per call; its pre-prototype form is quadratic).

| Rule | 794be1f | P1 | P2 | P3 | P1+P2+P3 |
|---|---|---|---|---|---|
| 1 ship-mode | 750 | **243** | (995, noise) | 728 | 229 |
| 2 safe-stock | 208 | 199 | 191 | 183 | 195 |
| 3 transfer-source-options | 91 | 91 | 88 | 90 | 89 |
| 4 warehouse-transfers | 1,750 | **1,212** | 1,584 | 1,440 | **989** |
| 5 delegated-buying | 553 | 496 | 551 | 512 | 457 |
| 6 moq-extra-quantity | 6,497 | 6,389 | **1,368** | 3,957 | **868** |
| 7 purchase-and-cancel | 614 | 621 | 620 | 597 | 584 |

The saved cases carry small run-wide tables; on real runs, where every item carries the route, lane
and ship-time tables, P2 and P3 save more, because both costs grow with the size of the input.

## P2: skip the input-node date walk when the rule has no input schema

**Where:** `core/engine/src/nodes/input/mod.rs`, the call
`dates::DeclaredDates::prepare(&ctx.input, ctx.node.schema.as_deref())` (upstream code, from upstream's
date-input feature).

**What it costs today:** every evaluation of every rule, including every sub-rule call from a decision
step, walks the whole input recursively. Without a schema the walk only looks for `Variable::Dynamic`
values (engine date values) to turn back into their source text, but it still visits every object and
array. Rule 6 calls a sub-rule once per projection with the whole projection list as input, so the walk
is quadratic in the number of projections: 6,497 -> 1,368 us/SKU with P2 alone.

**Prototype:** when `ctx.node.schema` is `None`, return the input unchanged without walking it.

**Behaviour risk:** a parent rule that passes engine date values (`Variable::Dynamic`, e.g. the result of
`d(...)` in an expression step) into a sub-rule whose input node has no schema. Today the sub-rule sees
the date's source text; with the prototype it sees the date value itself, so string functions on that
field, output serialisation and equality against text can differ.

**Durable options, for the owner to choose:**

1. Walk only when the value tree can hold a `Dynamic`: the walk is skipped for input parsed from JSON
   (the C API and HTTP paths never create `Dynamic` values), and kept for sub-rule calls whose parent
   produced dates. Needs a cheap "contains Dynamic" flag carried from where the input is built.
2. Walk only when a schema exists, and document that a sub-rule without an input schema receives date
   values as dates. Simplest; changes behaviour for the case above.
3. Leave the walk and fix rule 6's shape instead (the rule prototype reaches the same 868 us/SKU
   without any engine change).

## P3: no deep copy of expression results that read `$`

**Where:** `core/engine/src/nodes/expression/mod.rs`:

```rust
let value = match expression.value.contains('$') {
    true => value.deep_clone(),
    false => value,
};
```

**What it costs today:** every expression whose text contains `$` (reading an earlier key of the same
step) deep-copies its result, so an expression that returns or reshapes a large earlier object copies all
of it. Rule 6: 6,497 -> 3,957 us/SKU; rule 4: 1,750 -> 1,440.

**Prototype:** never deep-copy (`contains('$') && false`).

**Behaviour risk:** the copy protects against aliasing. Objects and arrays are shared by reference
(`Rc<RefCell<...>>`); `result.dot_insert` on a later key such as `a.b` writes into the object stored at
`a`. If an earlier key's result is the same object as `$.x` (for example `copy = $.x` followed by key
`x.y = 1`), without the copy both keys change. No PO Plan rule writes into a value that another key
still references, which is why the saved cases are identical, but the engine cannot know that in
general.

**Durable option:** copy-on-write. Copy only when a later key's `dot_insert` would write into a value
whose reference count shows it is shared with an earlier key (or the trace), instead of copying every
`$` result up front. That keeps today's semantics exactly and removes the copies that are never written
to. It touches `dot_insert` in `core/types`, so it needs its own tests (aliasing cases per key shape).

## Where the numbers come from

Measured 2026-11-07 on the PO Plan Daily benchmark (60,000 SKUs x 4 warehouses, module 1.0.3 = zen
794be1f). The patched engine, the switch per change and the per-case output comparison are in the
benchmark workspace (`after-bench/tools/zen-proto-patch.js`, `engx.js verify`), outside this repository.
