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
- Benchmarks and a profiling harness (`ms/speedbench`), not part of the shipped library.

### Changed

- Compiled decisions keep their graph, switch conditions, input fields and decision-table cell
  bytecode, so per-evaluation setup is done once per rule version.
- `$nodes` is built only for nodes that can read it. Isolates are reused per thread.
- Large objects and traces keep insertion order.
- JS function results convert each distinct key once.
- `compile()` works for engines created with a loader config.

### Fixed

- A panic inside an `extern "C"` function no longer aborts the host process; every export has a panic
  guard.
- The trace of a failed evaluation is no longer serialised as `null`.

### Removed

- Nothing. Every upstream export in `zen_engine.h` is unchanged.
