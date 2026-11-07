# Engine speed proposals P2 and P3 (not in this release)

Measured on the PO Plan Daily rules (7 decide rules, all without JavaScript) while looking for where the
engine spends time per SKU. Three engine changes were prototyped on a copy of this fork at 794be1f and
switched on one at a time:

- **P1, regex cache for `extract()` / `matches()`: shipped in this release** (see CHANGELOG).
- **P2 and P3: proposals only.** Each one changes behaviour in a case the PO Plan rules do not use but
  another rule might, so the durable form needs an owner decision. Both are described below with what was
  measured and what could break.

With P1, P2 and P3 all switched on, all 1,147 saved request cases of the 7 rules gave output identical to
794be1f (ship-mode 149, safe-stock 70, transfer-candidates 153, transfers 265, delegated-buying 103,
moq 264, purchase-and-cancel 143). None of those rules uses date values, so the cases do not exercise
the P2 risk below.

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
