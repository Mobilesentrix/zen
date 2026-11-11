# MobileSentrix fork of gorules/zen

Base: upstream tag `zen-engine-v2.1.6` (all crates 2.1.6). `git diff zen-engine-v2.1.6` shows every fork change to an
upstream file; new files are `ms_*.rs`, `ms/`, `ms-test/` and the `MS-*.md` documents.
The C contract is in [MS-FFI.md](MS-FFI.md); build and test commands are in [MS-BUILD.md](MS-BUILD.md).

## What it adds, and why

| Change | Where | Reason |
|---|---|---|
| Database node (custom node kind `database`): `{{ expr }}` placeholders bound as parameters, `{{#if}}` blocks, checked LIMIT/OFFSET, loop mode, `resultHandling`, pools on a private runtime | `ms/database` (crate `ms-zen-database`) | Rules query MySQL/StarRocks/Postgres without SQL injection; implemented as a `CustomNodeAdapter`, so the engine core is not patched for it |
| C constructors that install the database node; error codes 12 (database config) and 13 (panic) | `bindings/c/src/ms_database.rs`, `error.rs` | Embedding from Go through the C API |
| The editor's Lookup step (`databaseNode`) converted into the database node in Rust (custom query, or the builder's tables and conditions compiled to SQL), ported from the Go module with a 3,000-case parity test | `ms/database/src/editor/`, `ms/database/tests/editor_parity.rs` | Every embedder (Java, Rust) runs stored rules as the Rules Engine does, from one implementation |
| `ZenEngine.withDatabase(loader, customNode, databaseConfig, routeByInstance)` in the uniffi binding: database node installed, Lookup steps converted by every loader and by `createDecision`; without a config a Lookup step fails with "database node not available" | `bindings/uniffi/src/{engine,database,loader}.rs`, `ms/database/src/unavailable.rs` | Embedding from Java (the module of PO Plan) through the uniffi SDK |
| Panic guard on every `extern "C"` function | `bindings/c/src/guard.rs` | A panic that unwinds out of `extern "C"` aborts the host process |
| Compiled-decision cache for callback-loader engines (LRU, at most 4096 decisions: `DEFAULT_CACHED_DECISIONS`, or `CachedLoader::with_capacity` in Rust), `zen_engine_invalidate_decision` / `zen_engine_clear_decisions`; `compile()` works for loader-config engines | `bindings/c/src/{engine,loader,ms_cache}.rs`, `core/engine/src/loader/cached.rs` | The C binding never compiled decisions; every evaluation re-parsed and re-compiled the rule |
| `zen_engine_create_decision` returns a prepared decision, kept in a per-engine LRU | `bindings/c/src/ms_created.rs` | Created decisions (simulate) were evaluated uncompiled |
| Traced errors carry the partial trace and, on the failing node, a structured `error` | `bindings/c/src/ms_error.rs`, `core/engine/src/nodes/failure.rs` | Upstream serialises the trace of an error as `null` |
| Multi-thread C runtime (one worker); context parsed straight into a `Variable`; responses written into `malloc` memory | `bindings/c/src/{mt,ms_context,ms_alloc}.rs` | Concurrent cgo callers ran one at a time; one JSON tree instead of two; no extra copy of large responses |
| mimalloc (no THP) as the library allocator, a once-a-second purge thread, a fixed glibc mmap threshold | `bindings/c/src/ms_alloc.rs` | Memory returns to the OS after a burst of large evaluations |
| Compiled decisions keep their graph, switch conditions, input fields and decision-table cell bytecode; an evaluation works on a view of the shared graph | `core/engine/src/{model/decision_content.rs,decision_graph/view.rs,nodes/decision_table/cells.rs}` | Per-evaluation setup is done once per rule version |
| `$nodes` built only for nodes that can read it; isolates reused per thread; node context content behind `Rc` | `core/engine/src/{decision_graph/walker.rs,nodes/context.rs}`, `core/expression/src/isolate.rs` | Fewer allocations and atomic reference-count updates per evaluation |
| Independent database steps start as soon as their inputs are known, when their SQL only reads | `core/engine/src/decision_graph/early.rs`, `core/engine/src/nodes/statement.rs` | Waits on different lookups overlap; the walk order and results are unchanged; a statement that may write runs where and as often as the walk runs it |
| Large objects and traces keep insertion order (IndexMap) | `core/types/src/variable/map.rs`, `decision_graph/tracer.rs` | Output key order is the same on every run |
| JS function results convert each distinct key once | `core/engine/src/nodes/function/v2/serde.rs` | Arrays of objects with the same fields |
| `executionMode: "sequential"` for decision, decision-table and expression steps: items in order, a state carried from each item to the next (`stateField`, `statePath`, `maxIterations`, `stop`), failing items named by index | `core/engine/src/nodes/transform_attributes.rs`, `core/types/src/decision/mod.rs`, `nodes/failure.rs`, `workspace/graph/{analysis,cursor,enhance}.rs` | Rules where step N depends on step N-1 (running budgets, allocations, timelines, bisection) run as decision tables instead of JavaScript ([ADR 0006](docs/adr/0006-sequential-execution-mode.md)) |
| Expression list functions: `sort`, `sortBy`, `minBy`, `maxBy`, `unique`, `uniqueBy`, `countDistinct`, `groupBy`, `keyBy`, `join`, `first`, `last`, `reduce`, `scan` (accumulator `acc`); closure functions may take a trailing argument after the callback | `core/expression/src/functions/ms_list.rs`, `compiler/ms_list.rs`, `intellisense/ms_list_types.rs`; hooks in `functions/{mod,internal}.rs`, `compiler/{compiler,opcode}.rs`, `vm/vm.rs`, `parser/{parser,unary}.rs`, `intellisense/{completion,dependency,type_provider}.rs`, `slot/{classify,literals}.rs` | Rules sorted, ranked, grouped and kept running totals in JavaScript Function nodes; the semantics are in the `ms_list.rs` module comment, tests in `core/expression/tests/ms_list_functions.rs` |
| `extract()` / `matches()` reuse compiled regexes, per thread, at most 256 patterns (emptied when full); an invalid pattern is never cached | `core/expression/src/functions/internal.rs` (`compiled_regex`), tests in `core/expression/tests/ms_regex_cache.rs` | Upstream compiles the pattern on every call (~180 us); rules that parse or format text per SKU spent most of their time there. P2 and P3 are the next two rows ([docs/engine-speed-proposals.md](docs/engine-speed-proposals.md)) |
| Input node walks for engine date values only when one can exist: the first input node of an evaluation checks its own input once, and every date function creates its value through `DateValue::wrap`, which counts; a sub-rule call skips the walk while its input has no schema, the evaluation's input held no date and no date was made since | `core/engine/src/nodes/{extensions,input/mod}.rs`, `core/expression/src/dates.rs` (+ `functions/{date_method,internal}.rs`), tests in `core/engine/tests/ms_engine_speed.rs` | Upstream walks the whole input of every rule call; a rule that calls a sub-rule once per projection with the whole list walked it quadratically (PO Plan moq rule) |
| Expression results that read `$` are deep-copied only when a later write could reach them: a key with a dot, or a new key while a pending value holds `$` itself; the pending values then get the copy they would have had, in the result, in `$` and in the trace. `ending_variables` merges copy-on-write. What the node returns is unchanged | `core/engine/src/nodes/expression/mod.rs` (`Shared`), `decision_graph/walker.rs`, tests in `core/engine/tests/ms_engine_speed.rs` (aliasing cases per key shape; generated key sequences against output of zen 615aa9b) | Upstream-era code copied every `$` result up front, so a key that returns or reshapes a large earlier object copied all of it |
| Benchmarks and profiling harness | `ms/speedbench`, `ms/database/tests/{dbspeed,listing}.rs`, `core/engine/benches/ms_list_vs_function.rs` | Measuring the above; not part of the shipped lib |

Every upstream export in `zen_engine.h` is unchanged. Parity tests: `core/engine/tests/ms_compiled_parity.rs`
(compiled vs uncompiled), `ms_concurrent_steps.rs` (early start vs one by one), `error_trace.rs`.

## Build

Linux only, in Docker (`rust:1-bookworm`, glibc 2.36). The deliverable is the static library:

```bash
ms/scripts/docker-cargo.sh build --release -p zen-ffi          # target/release/libzen_ffi.a
```

The arm64 cross build, strip step, test commands (`zen-engine` must be tested with
`--features arbitrary_precision`, `zen-ffi` with `--no-default-features`) and the database
integration tests are in MS-BUILD.md. The Go wrapper (`ms-zen-go`) carries the built libs.

## Updating from upstream

1. `git fetch origin && git rebase <new upstream tag>` on the fork branch (upstream files are
   changed only where that diff shows; most fork code is in new files).
2. Resolve conflicts in the fork's hunks. For `Cargo.lock`, take upstream's version; the next
   cargo build adds the fork's dependencies (`mimalloc`, `hashlink`, `indexmap`, sqlx, …) back.
3. Run the tests from MS-BUILD.md (unit, parity, database integration, C smoke test), rebuild
   both libs, and copy them and `bindings/c/zen_engine.h` into ms-zen-go.
