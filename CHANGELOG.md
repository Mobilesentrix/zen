# Changelog

## Unreleased — Rule Engine v2

Base: upstream `zen-engine-v2.1.6`. See [FORK.md](FORK.md) for files and reasons.

### Fixed

- StarRocks Lookups with a subquery could return another request's row: StarRocks re-executes a
  prepared statement with the parameters of its first execution. On StarRocks every query is now
  prepared, executed and closed (one extra round trip, about 1 ms for a point lookup, 8 ms for a
  correlated subquery on a local FE). `docs/starrocks-prepared-statement-stale-params.md`.

### Added

- Database node (custom node kind `database`) for MySQL, StarRocks and Postgres: `{{ expr }}`
  placeholders bound as parameters, `{{#if}}` blocks, checked LIMIT/OFFSET, loop mode,
  `resultHandling`, connection pools on a private runtime.
- `maxLoopItems` (default 1000) per database connection bounds a loop-mode node.
- Compiled-decision cache (LRU, 4096 decisions) for engines with a callback loader, with
  `zen_engine_invalidate_decision` and `zen_engine_clear_decisions`. Decisions created for
  simulation are prepared and kept in a per-engine LRU.
- Parallel evaluation: the C runtime has a multi-thread worker, so concurrent callers no longer run one
  at a time.
- Early database steps: independent database steps whose SQL only reads start as soon as their
  inputs are known; a statement that may write runs in walk order, as often as the rule asks. Walk
  order and results do not change.
- Allocator: mimalloc with a once-a-second purge thread, so memory returns to the OS after bursts of
  large evaluations.
- Structured errors: error codes 12 (database config) and 13 (panic), and a traced error carries the
  partial trace and, on the failing node, a structured `error`.
- Response bodies are written into `malloc` memory, so the caller can read a large response without a
  copy. The request context is parsed straight into a `Variable`.
- `executionMode: "sequential"` for decision, decision-table and expression steps: items evaluated in
  order with a state carried between them, early `stop`, `maxIterations`; a failing item is named by
  its index (ADR 0006).
- Benchmarks and a profiling harness (`ms/speedbench`), not part of the shipped library.
- Expression list functions, so rules no longer need a Function node to sort, rank, group or keep a
  running total: `sort(array, 'desc'?)`, `sortBy(array, key, 'desc'?)` (stable, nulls last, an array
  key orders by several fields), `minBy` / `maxBy` (the element; first on ties), `unique`,
  `uniqueBy`, `countDistinct`, `groupBy` (object of arrays), `keyBy` (first element per key wins),
  `join(array, separator)`, `first`, `last`, `reduce(array, step, initial)` and
  `scan(array, step, initial)`; the step reads the accumulator as `acc`. Numbers stay decimal.
- The editor's Lookup step (`databaseNode`) is converted into the database node in Rust
  (`ms_zen_database::editor`: `prepare_decision_content`, `transform_database_nodes`), the same SQL
  and config as the Go module (parity test over 3,000 cases).
- uniffi: `ZenEngine.withDatabase(loader, customNode, databaseConfig, routeByInstance)` runs rules
  with Lookup steps (every loader kind and `createDecision` convert them); error
  `DatabaseConfigError`. Evaluations run inside the database de-duplication scope.

### Changed

- Compiled decisions keep their graph, switch conditions, input fields and decision-table cell
  bytecode, so per-evaluation setup is done once per rule version.
- `$nodes` is built only for nodes that can read it. Isolates are reused per thread.
- Large objects and traces keep insertion order.
- JS function results convert each distinct key once.
- `compile()` works for engines created with a loader config.
- `extract()` and `matches()` reuse compiled regexes (per thread, up to 256 patterns; the cache is
  emptied when full) instead of compiling the pattern on every call: about 180 us per call before. PO
  Plan ship-mode rule 750 -> 243 us/SKU, warehouse-transfers 1,750 -> 1,212 us/SKU, same outputs.
  Two further engine changes (P2, P3) are proposals only: `docs/engine-speed-proposals.md`.

### Fixed

- An invalid database configuration given from inside an async task is an error, not a panic
  (the database runtime was dropped in an async context).
- A panic inside an `extern "C"` function no longer aborts the host process; every export has a panic
  guard.
- The trace of a failed evaluation is no longer serialised as `null`.

### Removed

- Nothing. Every upstream export in `zen_engine.h` is unchanged.
