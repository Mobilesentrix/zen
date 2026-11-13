# Architecture decision records

Decisions behind the MobileSentrix changes to this fork of gorules/zen. The list of changes is in
[../../FORK.md](../../FORK.md); the C contract is in [../../MS-FFI.md](../../MS-FFI.md).

| ADR | Topic |
|---|---|
| [0001](0001-runtimes-and-concurrency.md) | Runtimes: database pools and the C evaluation runtime |
| [0002](0002-memory-allocation.md) | Allocator, purging and the memory shared with the C caller |
| [0003](0003-evaluation-hot-path.md) | Evaluation hot path: graph view, node data, isolate |
| [0004](0004-database-crate-layout.md) | Database crate layout |
| [0005](0005-fork-code-comments-and-lints.md) | Fork code comments and lints |
| [0006](0006-sequential-execution-mode.md) | Sequential execution mode (items in order, with a carried state) |
