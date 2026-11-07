# MS FFI contract — libzen_ffi with the database node

Base: gorules/zen tag `zen-engine-v2.1.6`.
Everything upstream in `bindings/c/zen_engine.h` is unchanged and still exported
(`zen_engine_new_golang`, `zen_engine_evaluate`, `zen_engine_evaluate_batch`, `zen_engine_free`,
expression/template helpers …). This document lists only what MobileSentrix adds.

Artefacts: `libzen_ffi.a` and `zen_engine.h` for linux_amd64, and the same lib cross-compiled
for linux_arm64 (same header, same link line: `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`).

## 1. Added functions

```c
/* Go: like zen_engine_new_golang (Go loader callback), plus the database node. */
struct ZenResult_ZenEngineStruct zen_engine_new_golang_with_database(
    const uintptr_t *maybe_loader,       /* cgo.Handle of the loader, NULL/0 = none          */
    const uintptr_t *maybe_custom_node,  /* cgo.Handle for NON-database custom nodes, NULL/0 */
    const char *database_config);        /* connection-map JSON, see §2, required            */

/* Go: like zen_engine_new_golang_with_loader_config, plus the database node. */
struct ZenResult_ZenEngineStruct zen_engine_new_golang_with_loader_config_and_database(
    struct ZenEngineLoaderConfig config,
    const uintptr_t *maybe_custom_node,
    const char *database_config);

/* Non-Go (plain C callback) variant. */
struct ZenResult_ZenEngineStruct zen_engine_new_with_loader_config_and_database(
    struct ZenEngineLoaderConfig config,
    ZenCustomNodeNativeCallback maybe_custom_node,   /* may be NULL */
    const char *database_config);
```

* Returns `ZenResult_ZenEngineStruct { result, error, details }`. On success `error == 0` and
  `result` is the engine; free it with the upstream `zen_engine_free(engine)` (this also shuts
  down the engine's database runtime and closes its pools).
* `database_config` is only read during the call; the caller keeps ownership and frees it.
  On error `details` is a heap C string the caller frees with the upstream `free`/`C.free`
  (same convention as every other `ZenResult`).
* Evaluate with the unchanged upstream `zen_engine_evaluate` / `zen_engine_evaluate_batch`.
* The static lib still references the two upstream Go callbacks, exactly like zen-go v2:
  `zen_engine_go_loader_callback` and `zen_engine_go_custom_node_callback` (must be exported
  by the Go side, as zen-go already does).

### Decision cache (ABI addition)

Engines created with a Go/native loader callback (`zen_engine_new_golang*`,
`zen_engine_new_native`, including the `*_with_database` variants) keep the compiled
decision of every key the loader returned: the loader callback is invoked **once per key**, and
later `zen_engine_evaluate` / `zen_engine_evaluate_batch` calls reuse the compiled graph.
Loader-config engines (`zen_engine_new_*_with_loader_config*`) build their compiled set at
creation (upstream `compile()`, which the upstream C binding never engages).

The callback-loader cache keeps at most 4096 decisions (`zen_engine::loader::DEFAULT_CACHED_DECISIONS`);
past that the least recently used decision is dropped and loaded again on its next evaluation.

**The embedder must tell the engine when a rule changes**, otherwise it keeps evaluating the
previous content:

```c
/* Drops the cached decision for `key`; the next evaluation reloads it through the loader.
   Loader-config engines recompile their whole set. Returns 0, or 1 (InvalidArgument) when
   `engine` or `key` is NULL / `key` is not UTF-8. */
uint8_t zen_engine_invalidate_decision(const struct ZenEngineStruct *engine, const char *key);

/* Drops every cached decision (rules refreshed from S3, loader swapped, …). Returns 0 or 1. */
uint8_t zen_engine_clear_decisions(const struct ZenEngineStruct *engine);
```

Both are safe to call while other threads evaluate: an in-flight evaluation finishes on the
content it started with, the next one sees the reload. `key` is only read during the call.
A load that was already in flight when the invalidation happened never re-inserts its result
(the cache carries a generation counter bumped by every invalidation).
(Go: wrap as `engine.InvalidateDecision(key)` / `engine.ClearDecisions()`; module-go calls
them from its rule-refresh path.)

### Created decisions (behaviour change, no ABI change)

`zen_engine_create_decision(engine, content)` returns a **prepared** decision: the graph is
compiled and its schemas resolved exactly as the loader path does, instead of being evaluated
uncompiled on every `zen_decision_evaluate`. The prepared graph is remembered **per engine** in
a bounded LRU (64 entries) keyed by the exact JSON text, so a repeated create of the same rule
(simulate, playground, batch of the same draft) reuses the compiled code: the second create
costs a hash + string compare, no parse. Only rule code is cached, never evaluation data;
a different text (any byte) is a miss. `zen_engine_clear_decisions` also drops this cache;
`zen_decision_free` only frees the handle. Error codes are unchanged: 1 for a NULL argument or
a policy document, 2 for text that is not valid UTF-8 JSON.

### Evaluation errors carry the partial trace (behaviour change, no ABI change)

When `zen_engine_evaluate`, `zen_engine_evaluate_batch` or `zen_decision_evaluate` is called
with `trace: true` and a node fails, the `NodeError` JSON in `details` has a `trace` object
instead of `"trace": null`: every node that completed (same shape as a successful traced
response: `input`, `output`, `performance`, `order`, `traceData`) plus the failing node with
its `input` (its `output` is null). The engine always collected this; upstream's error
serialiser used trace kind `None`, so the FFI dropped it. Untraced calls are unchanged (no
`trace` key payload). This is what a failed simulate needs to show the steps before the failure
and the inputs the failing expression saw.

### Error trace: the failing step says what failed (behaviour change, no ABI change)

In a traced error (above), the failing node's entry also has an `error` object. Only the failing
node has it (the key is absent on every other entry, and on every entry of a successful trace);
`source`, the error text, is unchanged; untraced calls are unchanged.

```json
"trace": {
  "in":   { "id": "in", "name": "Request", "order": 0, "input": null, "output": {...}, "traceData": null, "performance": "..." },
  "ok":   { "id": "ok", ..., "traceData": { "k": { "result": 1 } } },
  "fail": { "id": "fail", "name": "Margin", "order": 2, "input": { "sku": "1", "k": 1 }, "output": null,
            "traceData": { "k": { "result": 1 } }, "performance": "...",
            "error": {
              "kind": "missingValue",
              "message": "VM error: Opcode Subtract: Unsupported type",
              "part": "expression",
              "expression": "missing.price - cost",
              "key": "margin",
              "missing": ["missing.price", "cost"]
            } }
}
```

| field | always | meaning |
|---|---|---|
| `kind` | yes | `parse` (lexer/parser/compiler error, invalid node config or query template), `missingValue` (a value it needs is absent or null), `type` (a value has the wrong type, incl. "Expected an array"), `evaluation` (anything else: database error, function error, result handling) |
| `message` | yes | the underlying cause; the full placeholder text for a database placeholder |
| `part` | no | `expression` (expression row, `key` names it), `inputField` (the node's input field / loop input), `placeholder`, `query`, `config`, `subDecision` |
| `expression` | no | the exact text that failed: the expression, the input field, the `{{ }}` placeholder, the SQL |
| `key` | no | expression node: the output key of the failing row |
| `missing` | no | dotted paths the expression reads that were absent or null (a read through `x[0]` is reported as `x`) |
| `nodeId` | no | `subDecision`: the step inside the sub-decision that failed (its own entry, in this node's `traceData`, has the full `error`) |

A node without structure (function node, custom node callback, validation) still gets
`{kind, message}`; validation errors are `type`. Decision-table cells never fail a table
(upstream semantics: a failing input cell is "no match", a failing output cell skips the row),
so a decision-table failure is its input field / loop input (`part: "inputField"`). A type error
on null operands is reported as `missingValue`. The structure is only built on the failure
path, and for expression/decision-table/sub-decision nodes only when the call is traced.

### Error code added

| code | name | details JSON |
|---|---|---|
| 12 | `DatabaseConfigError` | `{"message": "..."}` — bad JSON, unknown field, unsupported URL scheme, duplicate alias, unknown `default`, or (with `connectEagerly`) an unreachable database |
| 13 | `Panic` | `{"message": "..."}` — a Rust panic was caught at the C boundary (every exported function runs under `catch_unwind`; a panic cannot abort the host process). The engine stays usable. Raw-pointer constructors (`zen_engine_new`, `zen_engine_new_golang`, `zen_engine_new_native`) return NULL instead; `zen_engine_invalidate_decision`/`zen_engine_clear_decisions` return 13; the `*_free` functions swallow it. |

A decision key containing a NUL byte (e.g. a `decisionNode` whose `key` holds `\u0000`) is a
loader error (`LoaderInternalError`, "decision key contains a NUL byte"), not a panic.

Codes 0–11 are upstream and unchanged (0 ok, 1 InvalidArgument, 2 StringNullError,
3 StringUtf8Error, 4 JsonSerializationFailed, 5 JsonDeserializationFailed, 6 IsolateError,
7 EvaluationError, 8 LoaderKeyNotFound, 9 LoaderInternalError, 10 TemplateEngineError,
11 LoaderConfigError). Database failures *during evaluation* (bad SQL, null placeholder,
unknown instance, connection refused) are node errors and come back from `zen_engine_evaluate`
as code 7 `EvaluationError`, details e.g.
`{"type":"NodeError","nodeId":"db","source":"Database query execution failed: Query execution error: ..."}`.

## 2. `database_config` JSON

```json
{
  "connections": {
    "ddd3956b-d0e7-428d-a62d-35b728a5bd23": {
      "url": "mysql://user:pass@starrocks-fe:9030/mobilesentrix_dev",
      "aliases": ["starrocks"],
      "maxConnections": 16,
      "minConnections": 0,
      "acquireTimeout": 30000,
      "idleTimeout": 600000,
      "maxLifetime": 1800000,
      "statementCacheCapacity": 100,
      "connectEagerly": false,
      "maxRows": 10000,
      "maxLoopItems": 1000,
      "queryTimeout": 30000
    },
    "pg-main": { "url": "postgres://user:pass@pg:5432/app" }
  },
  "default": "ddd3956b-d0e7-428d-a62d-35b728a5bd23",
  "workerThreads": 2
}
```

* Keys of `connections` are the editor's `instance` ids; `aliases` lets a node's
  `instanceName` (or any other label) resolve to the same pool.
* URL schemes: `mysql://`, `mariadb://`, `starrocks://` (all MySQL protocol; StarRocks-safe
  connect: sqlx's `SET sql_mode=...` is suppressed), `postgres://`,
  `postgresql://`. TLS via URL params (`ssl-mode=` / `sslmode=`), rustls, no OpenSSL.
* **Server flavor is detected, not configured**: the first connection a
  MySQL-protocol pool opens runs `SELECT current_version()` (a StarRocks function; MySQL and
  MariaDB answer with an "unknown function" error) and the pool remembers the answer; one
  round trip per new connection, none per query. It decides how a non-integer number
  placeholder is bound: MySQL/MariaDB get an exact NEWDECIMAL parameter; StarRocks rejects that
  parameter type ("unknown mysql type code 246"), so it gets the number's plain text
  (`"12.5"`), which StarRocks folds into an exact literal of the column's type — the plan keeps
  `cost = 12.5` and its pruning (a DOUBLE parameter would become `CAST(cost AS DOUBLE) = …`).
  Integers stay LONGLONG everywhere. DECIMAL / NUMERIC result columns are decoded as exact JSON
  numbers (arbitrary precision in the FFI build), never as strings.
* Timeouts are milliseconds.
* `default`: used by nodes without `instance`. If omitted and exactly one connection exists,
  that one is the default.
* Pools are created **once per engine**, lazily by default (an unreachable instance does not
  stop rules that never use it). `connectEagerly: true` opens one connection at creation and
  fails with code 12 if it cannot.
* `maxRows` (default **10000**, minimum 1): a query that returns more rows than this fails
  with a node error naming the connection and the limit (`query returned more than N rows
  (connection "x" maxRows); add a LIMIT or raise maxRows`). Rows are streamed and counted, so an
  unbounded `SELECT` never materialises in the engine process. Raise it per connection if a rule
  legitimately needs more.
* `queryTimeout` (milliseconds, optional, no default): bounds one statement's execution.
  On expiry the node fails with `query exceeded the queryTimeout of N ms on connection "x"`
  and the connection is **discarded** (the statement is still running on it; the pool opens a
  replacement, so the next query does not inherit the wait — the server ends the session when
  it notices the closed socket). `acquireTimeout` only bounds *obtaining* a connection; set
  `queryTimeout` to bound statement execution.
* `testBeforeAcquire` (default true, sqlx's default): ping a pooled connection before each use.
  `false` removes one server round trip per query; then keep `idleTimeout` below the server's
  idle disconnect (StarRocks `wait_timeout`, default 8 h; Postgres has none) and set
  `maxLifetime`, so a dead connection is never handed out.
* `sessionVariables` (object, default empty): `SET <name> = <value>` run on every new connection
  (simple-query protocol, before it joins the pool). Names must be identifiers (`[A-Za-z_][A-Za-z0-9_.]*`,
  rejected with code 12 otherwise); plain integers and decimals (`-12`, `0.25`) and
  `true/false/on/off` are sent bare, everything else as a string literal quoted for the server
  (MySQL/StarRocks also escape backslashes, Postgres does not) — a value can never change the
  statement's shape.
  A variable the server rejects makes every connect of that pool fail; the node error names the
  statement (`session variable statement \`SET x = 1\` failed: …`) once `acquireTimeout` runs
  out, so set `connectEagerly: true` to catch it at engine creation (code 12). StarRocks example:
  `{"enable_short_circuit": "true"}` (point lookups on Primary Key tables); Postgres example: `{"statement_timeout": "2500"}`.
* `dedupe` (default true): inside **one evaluate call**, a second query with the same connection,
  SQL text and bound values reuses the rows of the first (one round trip). Only read-only
  statements (see "Read-only statements" in §3) are shared; any other statement runs every time. The table lives in a
  tokio task-local that `zen_engine_evaluate` / `zen_engine_evaluate_batch` / `zen_decision_evaluate`
  open per call and drop on return — nothing is kept across requests (no data
  caching: the data behind the rules changes continuously). Rust embedders opt in with
  `ms_zen_database::with_evaluation_scope(engine.evaluate(..))`; without it every node queries.
* `maxLoopItems` (default **1000**, minimum 1): a loop-mode node over more items fails before any
  query, with `loop of N items exceeds maxLoopItems (M) of connection "x"`. No corpus rule has a
  database node in loop mode; the largest array in any recorded request is 792 items.
* `workerThreads`: threads of the engine's private database I/O runtime (default 2). Unknown
  fields are rejected (typos fail loudly).
* Freeing the engine closes its pools (bounded 5 s grace) on a background thread, then stops the
  runtime; `zen_engine_free` itself does not block.

### Trust model

A rule's `instance` may name **any** connection (or alias) in the engine's map, and its
`query` is arbitrary SQL on that connection. The connection map therefore *is* the allow-list:
whoever can author rules for an engine can run SQL on every connection that engine is given,
with that connection's database privileges. Consequences for the embedder:

* configure only the connections the engine's rules are allowed to touch, and give each a
  database user with the minimum privileges (read-only where possible); one engine per trust
  domain if rule authors differ;
* the editor-side `instance` id is just a name — there is no per-rule allow-list in the engine,
  and `aliases` only add names for the same pool;
* values from the evaluation input never become SQL text (§4), so request callers cannot
  escalate beyond what the rule author wrote.
* `{"connections":{}}` is valid (engine with the node installed but no databases).

## 3. JDM node shape the engine accepts

zen 2.x has no `databaseNode` type; the Go side must rewrite each editor `databaseNode` into a
`customNode` with kind `"database"` (no SQL generation in Rust). `config` is either the output
of module-go `TransformDatabaseNodes` or the raw editor content — both are accepted:

```json
{ "id": "db", "name": "rows", "type": "customNode",
  "content": { "kind": "database", "config": {
      "instance": "ddd3956b-...",
      "query": "select ... where 'us' = {{site}} and sku in ({{skus}})",
      "outputPath": "rows",
      "passThrough": true,
      "inputField": null,
      "executionMode": "single" } } }
```

| field | meaning | default |
|---|---|---|
| `query` | SQL with `{{ expr }}` placeholders | — |
| `useCustomQuery` + `customQuery` | editor shape, used when `query` is absent | — |
| `instance` | connection name or alias | config `default` |
| `outputPath` | dot path the result is written to (`null`/`""` = default) | `"dbResult"` |
| `passThrough` | merge result into the node input | `true` |
| `inputField` | expression selecting the node input | whole input |
| `executionMode` | `"single"` or `"loop"` (one query per element of `inputField`) | `"single"` |

`selectedTables` / `whereClauseGroup` are *not* interpreted in Rust: module-go keeps building
that SQL. Other fields (`engine`, `database`, `instanceName`, …) are ignored.
Result: `{"rows": [ {col: value…} ], "rowsCount": n}` placed at `outputPath`; trace data is the
same object (array of them in loop mode). Every other custom-node kind goes to the Go callback.

### Read-only statements

When the decision is compiled, each database step's query is classified: it is read-only when its
first keyword is `SELECT` or `WITH`, no word in it is a data-changing or locking keyword (`INSERT`,
`UPDATE`, `DELETE`, `MERGE`, `INTO`, `CREATE`, `CALL`, `LOCK`, … — so data-modifying CTEs,
`SELECT … INTO` and `SELECT … FOR UPDATE` are not), and it is one statement. Only read-only steps
start ahead of the walk, run loop items concurrently (identical items sent once) and share
identical queries inside an evaluation. Any other step runs in walk order, exactly as often as the
rule asks, and a loop stops at its first failing item. Keywords inside string literals also count,
so a read can be classified as a write (it then just runs in walk order), never the reverse. Every
corpus query is read-only.

### `resultHandling` (additive)

What the node does with the rows the query returned (the editor's "If it finds…" setting).
Absent, `null` or `{}` = the plain `{rows, rowsCount}` output, byte for byte.

```json
"resultHandling": {
  "none":    "continue" | "error" | "empty",     // default continue
  "noneMessage": "Not found",                    // used by none=error
  "one":     "all" | "use",                      // default all
  "several": "all" | "error" | "first",          // default all   (alias: "many")
  "severalMessage": "Duplicates found"           // used by several=error (alias: "manyMessage")
}
```

| rows | setting | output at `outputPath` |
|---|---|---|
| 0 | `none: continue` | `{"rows": [], "rowsCount": 0}` (default) |
| 0 | `none: empty` | `{"rowsCount": 0}` — every column reads as `null` downstream |
| 0 | `none: error` | node error, `source` = `noneMessage` |
| 1 | `one: all` | `{"rows": [row], "rowsCount": 1}` (default) |
| 1 | `one: use` | `{…row, "rowsCount": 1}` — the row object itself |
| ≥2 | `several: all` | `{"rows": [...], "rowsCount": n}` (default) |
| ≥2 | `several: first` | `{…row0, "rowsCount": n}` |
| ≥2 | `several: error` | node error, `source` = `severalMessage` |

`rowsCount` is always present (it overwrites a column of that name). The trace data stays the
raw `{rows, rowsCount}`. In loop mode the setting applies to every item; the first failing item
fails the node. An unknown value is a node error (`Invalid database node config: unknown
variant 'stop' …`). Aggregation (sum/count/min/max/avg, GROUP BY) is done in SQL by the UI
compiler, not here; single-row semantics usually mean `one: use` together with
`several: first|error`.

**Error shape** (code 7 from `zen_engine_evaluate`, one envelope item from `evaluate_batch`):

```json
{"type":"NodeError","nodeId":"db","source":"Not found","trace":null,
 "details":{"reason":"none","rowsCount":0}}
{"type":"NodeError","nodeId":"db","source":"Duplicates found","trace":null,
 "details":{"reason":"several","rowsCount":3,"rows":[{…},{…},{…}]}}
```

`details` is added by the FFI only for these two errors (`rows` holds at most 10 rows); every
other node error is serialised exactly as upstream. The embedder must forward `resultHandling`
unchanged when it builds the custom-node config (module-go does).

## 4. Placeholders

`{{ expr }}` is a zen expression evaluated against the node input (including `$nodes`) and
**bound as a query parameter** — never spliced into SQL. `?` (MySQL) / `$n` (Postgres) markers
are generated. Arrays expand to `?, ?, ?`. Strings → text, integers → BIGINT, other numbers →
DECIMAL, booleans → BOOL. Rejected at parse time (node error, message names line/column):
unterminated / empty / nested `{{`, placeholder inside a quoted string or quoted identifier,
placeholder after `FROM`/`JOIN`/`INTO`/`UPDATE`/`TABLE`/`AS`/`ORDER BY`/`GROUP BY`/`.`, literal
`?`/`$1` mixed with placeholders, invalid expression. Rejected at bind time: null (use `?? default`),
empty array (guard: `{{ len(ids) == 0 ? [-1] : ids }}`), arrays with null/nested/object elements,
objects, dates (use `.format('%Y-%m-%d %H:%M:%S')`), more than 65 535 bind values.

## 5. Linking (cgo)

```
#cgo linux,amd64 LDFLAGS: -L${SRCDIR}/../out/libzen_ffi/linux_amd64 -lzen_ffi -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
```
(`cargo rustc --release -p zen-ffi -- --print native-static-libs` →
`-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`; a plain `cc … -lm -ldl -lpthread` link also works,
see `ms-test/ffi_smoke.c`, which passes against the shipped lib.)
Built on Debian bookworm (glibc 2.36) — link in an image with glibc >= 2.36
(e.g. `golang:1.x-bookworm`). See MS-BUILD.md for the exact native-static-libs line.

## 6. Threading

The engine handle is thread-safe (same as upstream). Each engine owns a small multi-thread
Tokio runtime that drives its pools; `zen_engine_evaluate` may be called concurrently from
many goroutines/threads. Create one engine per process (or per config) and reuse it — creating
an engine per request would create pools per request.
