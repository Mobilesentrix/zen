# Rules Engine changes, 7–12 November 2026 (zen)

This one commit brings `master` from `d9b08bf2` (merge of PR #8, rule-engine-v2) to the code of
`15a64965` (merge of PR #15). It replaces PRs #10, #11, #12, #13, #14 and #15. The final tree is
byte-for-byte the tree of `15a64965`, which is the code that was built and used by PO Plan. Nothing
was added, removed or rewritten while collapsing; only the history is shorter, so the work can be
reviewed as one change.

## A note to the Rules Engine team

These PRs were merged by the PO Plan owner (`wjavedMS`) with admin rights during the PO Plan
release, without your review. That should not have happened without you. This file explains each
change, why PO Plan needed it, what it costs and what to watch, so you can review it now. If you
want any part changed or reverted, say so and it will be done on a branch for your review.

Dates are US Eastern. PR #10 was merged on 6 November at 21:51 (7 November UTC); the last, PR #15,
on 11 November at 00:05.

## Changes

### 1. StarRocks: prepared statements are never re-executed

- **What changed:** `ms/database/src/connection.rs` — new `reuses_prepared_statements(flavor)`; the
  MySQL-protocol query now calls `.persistent(false)` on StarRocks and `.persistent(true)` on
  MySQL/MariaDB. `config.rs` comment: `statementCacheCapacity` does not apply to StarRocks.
  `ms/scripts/docker-cargo.sh` passes `MS_TEST_STARROCKS_URL` into the container.
- **Why:** StarRocks (reproduced on 3.5.20 and 4.1.4, also with a raw binary-protocol client)
  re-executes a prepared statement that has a subquery with the parameters of its *first*
  execution. A Lookup on a reused connection could return another request's row. PO Plan saw
  wrong Lookup rows.
- **Benefit:** Lookups with subqueries return the row asked for.
- **Downside / watch:** one extra round trip per StarRocks query. CHANGELOG states about 1 ms for a
  point lookup and 8 ms for a correlated subquery on a local FE. MySQL, MariaDB and Postgres keep the
  statement cache. If StarRocks fixes the bug upstream, this can be relaxed (the test
  `upstream_starrocks_reexecuted_statement_keeps_first_parameters` will start failing and tell you).
- **Blocker removed:** wrong Lookup rows in PO Plan rules on StarRocks.
- **Tests:** `ms/database/tests/starrocks_prepared.rs` (needs a live StarRocks FE, ignored by
  default; command in `MS-BUILD.md`). Repro and upstream report draft:
  `docs/starrocks-prepared-statement-stale-params.md`.
- **PRs:** #10 (reverted), #12.

### 2. Sequential execution mode (`executionMode: "sequential"`)

- **What changed:** a third value of `TransformExecutionMode` (`core/types/src/decision/mod.rs`)
  with new fields `stateField`, `statePath`, `maxIterations` (default `SEQUENTIAL_MAX_ITERATIONS` =
  100,000). Runtime in `core/engine/src/nodes/transform_attributes.rs` (same path as loop mode);
  failures carry `itemIndex` (`nodes/failure.rs`); graph analysis and cursor support in
  `workspace/graph/{analysis,cursor,enhance}.rs`. Design: `docs/adr/0006-sequential-execution-mode.md`.
- **Why:** several PO Plan rules walk ordered rows where row N depends on row N-1 (budgets consumed in
  warehouse order, timelines, an 80-step bisection, transfer balancers). Loop mode evaluates items
  independently, so these were written as JavaScript Function nodes (1.5–5 ms each).
- **Benefit:** these rules can be decision tables / expressions. Each item gets `{...item, state}`;
  an output `state` is merge-patched into the carried state; `stop: true` ends early.
- **Downside / watch:** new surface in the JDM format that the editor and other embedders must
  understand. A failing item fails the whole step (error text starts `item <index>: `).
  `maxIterations` reached without `stop` is an error. `outputPath` or `statePath` must be set.
- **Blocker removed:** removing JavaScript Function nodes from PO Plan rules.
- **Tests:** `core/engine/tests/sequential.rs`, `core/engine/tests/workspace_graph.rs` additions,
  benchmark `core/engine/tests/ms_sequential_bench.rs`. PR #12 reports zen-engine 552 passed,
  0 failed, 3 ignored.
- **PRs:** #10 (reverted), #12.

### 3. Expression list functions

- **What changed:** new functions `sort`, `unique`, `join`, `first`, `last` (internal) and closures
  `sortBy`, `minBy`, `maxBy`, `uniqueBy`, `countDistinct`, `groupBy`, `keyBy`, `reduce`, `scan`.
  Implementation in `core/expression/src/functions/ms_list.rs` and `compiler/ms_list.rs`; hooks in
  `functions/{mod,internal}.rs`, `compiler/{compiler,opcode}.rs`, `vm/vm.rs`,
  `parser/{parser,unary}.rs`, `intellisense/*`, `slot/{classify,literals}.rs`. The parser now lets a
  closure take a trailing argument after the callback (sortBy direction, reduce/scan initial value).
  `reduce`/`scan` steps read the accumulator as `acc`. Numbers stay decimal.
- **Why:** rules sorted, ranked, grouped and kept running totals in JavaScript Function nodes.
- **Benefit:** the same work in the expression VM; editor completions and type hints included.
- **Downside / watch:** these names are now built-in functions; check whether the editor or any
  stored rule uses them differently. Inside a `reduce`/`scan` step, `acc` means the accumulator.
  Semantics (stable sort, nulls last, first element on ties, first wins in `keyBy`) are documented
  in the `ms_list.rs` module comment; please review them, they are now a contract.
- **Blocker removed:** removing JavaScript Function nodes from PO Plan rules.
- **Tests:** `core/expression/tests/ms_list_functions.rs` with `tests/data/ms_list.csv`,
  `tests/data/analysis/ms_list.toml` and `completions.toml` additions; benchmark
  `core/engine/benches/ms_list_vs_function.rs`.
- **PRs:** #10 (reverted), #12.

### 4. Regex cache for `extract()` and `matches()`

- **What changed:** `compiled_regex` in `core/expression/src/functions/internal.rs`: a per-thread
  cache of up to 256 compiled patterns, emptied when full. An invalid pattern is never cached.
- **Why:** upstream compiles the pattern on every call (about 180 µs). PO Plan rules that parse text
  per SKU spent most of their time there.
- **Benefit:** PR #12 reports PO Plan ship-mode rule 750 → 243 µs/SKU, warehouse-transfers
  1,750 → 1,212 µs/SKU, same outputs.
- **Downside / watch:** memory of up to 256 compiled regexes per thread. A rule set with more than
  256 distinct patterns per thread will clear the cache repeatedly (correct, just slower).
- **Tests:** `core/expression/tests/ms_regex_cache.rs`.
- **PRs:** #12.

### 5. Lookup (database) steps in every embedder, incl. Java (uniffi)

- **What changed:**
  - `ms/database/src/editor/` (new): the editor `databaseNode` → engine `customNode` (kind
    `database`) conversion in Rust, ported from the Go module (`prepare_decision_content`,
    `prepare_decision_value`, `transform_database_nodes`, `InstanceRouting`, `PrepareError`).
  - `ms/database/src/unavailable.rs` (new): with no database config, a Lookup step fails with
    "database node not available: …" (the Go module's text); other custom nodes go to the fallback.
  - `ms/database/src/databases.rs`: an invalid config given from inside an async task returns an
    error instead of panicking (owned runtime is shut down with `shutdown_background`).
  - `bindings/uniffi`: new constructor `ZenEngine.withDatabase(loader, customNode, databaseConfig,
    routeByInstance)`; every loader kind (callback, static, filesystem, zip) and `createDecision`
    convert Lookup steps; evaluations run inside `with_evaluation_scope` (database de-duplication);
    new error `DatabaseConfigError` (names connections, never URLs). New deps: `ms-zen-database`,
    `zip`. The existing constructor is unchanged in behaviour.
  - The C binding (Go module) is unchanged.
- **Why:** PO Plan embeds the engine through the Java module and needs rules with Lookup steps (for
  example `po-plan/1-ship-mode-sea-or-air`) to run in-process instead of over HTTP.
- **Benefit:** one Rust implementation of the conversion for every embedder.
- **Downside / watch:** there are now two implementations of the conversion (Go module and Rust).
  They are kept equal only by the parity test; any change to the Go `TransformDatabaseNodes` must be
  mirrored. The parity harness needs Go (`ms/database/tests/editor_parity/go/run.sh`) to regenerate
  `expected.jsonl`. `ms-ruleengine-module-java` 1.1.0 pins a commit of `feature/database-embedders`.
- **Blocker removed:** Lookup rules could not run inside the PO Plan Java process.
- **Tests:** PR #13 reports `cargo test -p ms-zen-database -p zen-uniffi -p zen-engine`: 660 passed,
  0 failed, 45 ignored (ignored ones need a live database), including
  `editor_parity::conversion_matches_the_go_module` (3,000 generated cases,
  `ms/database/tests/editor_parity/`), `editor/tests.rs`, and
  `databases::tests::invalid_config_inside_an_async_context_is_an_error`.
- **PRs:** #13 (supersedes the closed, unmerged #9).

### 6. FLOAT / FLOAT4 decoded as their shortest decimal

- **What changed:** `ms/database/src/decode.rs`: new `float32(v)` converts through the shortest
  decimal text (`v.to_string().parse::<f64>()`) instead of `f64::from(v)`, for MySQL/StarRocks
  `FLOAT` and Postgres `FLOAT4`.
- **Why:** a stored `0.07` came back as `0.07000000029802322`; the Rules Engine HTTP backend returns
  `0.07`. On the PO Plan dev Daily of 10 November, 12,737 CA plan rows differed from the HTTP run in
  daily sales (US, UK, EU matched).
- **Benefit:** embedded and HTTP results match.
- **Downside / watch:** results of `FLOAT` columns change for every embedder (now the decimal you
  would read in a SQL client). `DOUBLE`/`FLOAT8` are unchanged.
- **Tests:** PR #14: `decode` unit tests, 4 passed, incl. `float32_is_its_shortest_decimal`
  (0.07, 0.03, 42.5, NaN, infinity).
- **PRs:** #14.

### 7. Engine speed: date walk skip (P2) and copy-on-write `$` results (P3)

- **What changed:**
  - P2: `core/engine/src/nodes/input/mod.rs` walks the input for engine date values only when a
    schema is present, the evaluation's own input held a date, or a date was made since the
    evaluation started. Every date is now created through `DateValue::wrap`, which counts
    (`core/expression/src/dates.rs`, `functions/{date_method,internal}.rs`,
    `nodes/extensions.rs`).
  - P3: `core/engine/src/nodes/expression/mod.rs` (`Shared`): an expression result that reads `$` is
    deep-copied only when a later write could reach it (a dotted key, or a new key while a pending
    value holds `$` itself). `ending_variables` merges copy-on-write (`decision_graph/walker.rs`).
- **Why:** a PO Plan rule that calls a sub-rule once per projection with the whole list walked that
  list quadratically (moq rule); `$` results were copied up front even when nothing could change them.
- **Benefit:** PR #15 reports moq rule 6,400 → 960 µs/SKU, warehouse-transfers 1,380 → 1,145 µs/SKU,
  other rules within 5 percent. (CHANGELOG says 6,500 for the moq starting figure; the PR says 6,400.)
- **Downside / watch:** both are subtle invariants. P2 relies on *every* date value being created
  through `DateValue::wrap`; a new date function that bypasses it would skip a needed walk. P3
  relies on the "could a later write reach it" rule; any change to expression-node key handling
  should rerun `ms_engine_speed.rs`. Proposal text and alternatives: `docs/engine-speed-proposals.md`.
- **Tests:** `core/engine/tests/ms_engine_speed.rs` (aliasing cases per key shape, date cases, 260
  generated key sequences compared with the output of `615aa9b`, golden file
  `tests/data/ms_expression_alias_golden.txt`). PR #15 reports the zen-engine full suite
  (`--features arbitrary_precision`), zen-expression and zen-types green, and all 1,147 saved PO Plan
  request cases of 7 rules identical before and after.
- **PRs:** #15.

### 8. Documentation only

`CHANGELOG.md`, `FORK.md` (new rows for every change above), `MS-FFI.md` (`stateField` part,
`itemIndex`, StarRocks note, Rust conversion), `MS-BUILD.md` (StarRocks test), `docs/adr/0006-…`,
`docs/adr/README.md`, `docs/engine-speed-proposals.md`,
`docs/starrocks-prepared-statement-stale-params.md`.

## Behaviour changes and compatibility

- **JDM:** `executionMode` accepts `"sequential"`, with new optional fields `stateField`,
  `statePath`, `maxIterations`. Older engines will not accept rules that use it.
- **Expressions:** 14 new function names; closures accept a trailing argument; `acc` inside
  `reduce`/`scan`. Rules that use them will not run on an older engine.
- **Errors / trace:** new `part: "stateField"` and `itemIndex` on failed sequential steps.
- **Database results:** `FLOAT`/`FLOAT4` values change (shortest decimal). StarRocks queries are no
  longer cached as prepared statements (`statementCacheCapacity` ignored there).
- **Lookup without config (uniffi `withDatabase`):** fails with "database node not available".
- **uniffi API:** new constructor `withDatabase`, new error variant `DatabaseConfigError`
  (Kotlin/Java `when` over `ZenError` must handle it). Existing constructor and C API unchanged;
  every upstream export in `zen_engine.h` is unchanged.
- **Dependencies:** `zen-uniffi` now depends on `ms-zen-database` and `zip` (`Cargo.lock`, +2 lines).
- **Versions:** no crate version was bumped; crates stay at `2.1.6`.
- **Answers:** P2 and P3 are meant to give identical answers and traces (see tests above).

## Replaced PRs and old merge commits

| PR | Title | Old merge SHA | What happened |
|---|---|---|---|
| #10 | Engine release: list functions, sequential loop, StarRocks stale-parameter fix | `794be1f0` | Backed out by #11 and re-released in #12 |
| #11 | Revert weekend engine release (re-released as one combined release) | `1fb4be8d` | The back-out of #10; net effect none |
| #12 | Engine release 2026-11-08: list functions, sequential mode, StarRocks fix, regex cache | `1f89872a` | Kept (topics 1–4) |
| #13 | feat(database): run the editor's Lookup steps in every embedder | `c231640d` | Kept (topic 5) |
| #14 | fix(database): decode FLOAT/FLOAT4 as their shortest decimal | `615aa9b4` | Kept (topic 6) |
| #15 | perf(engine): input date walk only when a date can exist; copy-on-write for $ results | `15a64965` | Kept (topic 7); its tree is this commit's tree |

The old commits leave `master` history but stay reachable: the PR refs (`refs/pull/<n>/head`) and
these branches still point at them: `release/engine-2026-11-07` (`06bec7dc`),
`revert/weekend-engine-release` (`a315c62a`), `release/engine-2026-11-08` (`5f2abead`),
`feature/database-embedders` (`c0775a65`), `fix/float32-shortest-decimal` (`1f6f338c`),
`perf/engine-p2-p3` (`ee84703a`). Consumers that pin one of these SHAs (ms-zen-go `v2.1.6-ms.17`
built from the #12 code; `ms-ruleengine-module-java` 1.1.0 pinning `feature/database-embedders`)
keep working, but should move to the new collapsed commit. Please keep those branches until they do.

## Versions

- **Tags made in the window:** none. The repository has no tags (checked with
  `gh api repos/Mobilesentrix/zen/tags`, empty) and no GitHub releases. Nothing is superseded.
- **Downstream versions built from this code** (other repos): ms-zen-go `v2.1.6-ms.17` (per PR #12)
  and `ms-ruleengine-module-java` 1.1.0 (per PR #13).
- **Proposal:** tag the collapsed merge on `master` as the fork's first tag, for example
  `zen-engine-v2.1.6-ms.1` (upstream base `zen-engine-v2.1.6` plus a fork counter), after your review.
  The name is your call; no tag will be created without you.

## How to verify

```bash
# 1. The collapsed commit has exactly the deployed tree (no output = identical)
git diff 15a64965 <collapsed-commit>

# 2. What changed against the pre-window base
git diff --stat d9b08bf2 <collapsed-commit>

# 3. Tests (Docker, linux/amd64; see MS-BUILD.md)
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-engine --features arbitrary_precision
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-expression
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-types
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p ms-zen-database
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-uniffi
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-ffi --no-default-features

# 4. Focused tests for this change
ms/scripts/docker-cargo.sh test -p zen-engine --features arbitrary_precision --test sequential --test ms_engine_speed
ms/scripts/docker-cargo.sh test -p zen-expression --test ms_list_functions --test ms_regex_cache
ms/scripts/docker-cargo.sh test -p ms-zen-database --test editor_parity
ms/scripts/docker-cargo.sh test -p ms-zen-database --lib decode

# 5. StarRocks stale-parameter test (needs a StarRocks FE; full recipe in MS-BUILD.md)
MS_DOCKER_NETWORK=ms-zen-sr MS_TEST_STARROCKS_URL='mysql://root@ms-zen-starrocks:9030' \
  ms/scripts/docker-cargo.sh test -p ms-zen-database --test starrocks_prepared -- --include-ignored --nocapture

# 6. Release library (as consumed by ms-zen-go)
ms/scripts/docker-cargo.sh build --release -p zen-ffi
```
