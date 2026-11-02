# 5. Fork code in upstream files carries plain comments, and upstream lints stay

## Status

Accepted

## Context

A rebase needs to find fork code in upstream files. A prefix on every fork comment repeats what `git diff zen-engine-v2.1.6` already shows, and comments comparing fork code with upstream describe history rather than the code.

## Decision

Comments on fork-added lines describe the code. `git diff zen-engine-v2.1.6` is how fork changes are found. Reasoning lives in these records.

Clippy on fork code: taken are `map_or`, `f64::from`, merged match arms, `Option<&str>`, `# Errors` docs on `Databases`, `QueryTemplate::parse` and `bind`, and `cast()`. Declined are `must_use_candidate`, `doc_markdown` on product names, `needless_pass_by_value` on public signatures, `finish_non_exhaustive`, and `f64` display casts in benchmarks: they change an exported signature or `Debug` output, or add noise.

Warnings on upstream lines stay, so the fork stays easy to rebase: the `zen-ffi` warnings (`engine.rs`, `languages/native.rs`, `languages/go.rs`, `loader.rs`, `result.rs`), `too_many_lines` in `core/engine`, and `large_enum_variant` on `Repr` in `core/types/src/variable/map.rs`, where boxing would change allocations on the hot path. `is_read_only_sql` in `core/engine/src/nodes/statement.rs` stays one function of about 65 lines of lexing.

## Consequences

- `zen-ffi`, `zen-engine`, `zen-expression` and `zen-types` keep their upstream warnings.
