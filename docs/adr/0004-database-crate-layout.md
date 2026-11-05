# 4. The database crate is split by responsibility and passes one query step value

## Status

Accepted

## Context

`pool.rs`, `adapter.rs` and `template.rs` were over 800 lines and mixed topics.

## Decision

`ms/database/src` is split into `error`, `connection` (one connection and running a query), `pool` (sqlx setup), `databases` (runtime and registry), `query_step` (bind and run for a node), `result_handling` (row shaping), `template/parser` and `template/guard`. The modules were already private and the re-exports in `lib.rs` keep the public paths.

`QueryStep`, a `Copy` struct of references, replaces the repeated `(sql, template, label, conn)` parameters. Stages return `Box<FailedNode>` or `NodeError` in the same order as before, so error text does not change. It is free on the evaluate path: stack references and static dispatch.

Loop dedupe: `assign_queries` moves each `BoundQuery` into the unique list and maps each item to `Result<query index, failure>`, so the `unreachable!` arm goes away with one clone per item. The set of queries, their order and which failure is reported stay the same.

`tests/databases/` is a directory target (`main.rs` plus topic modules), so `--test databases` still works.

## Consequences

- Test names carry a module prefix; filters by name still match, because they match substrings.
