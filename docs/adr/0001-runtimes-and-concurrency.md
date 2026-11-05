# 0001. Runtimes: database pools and the C evaluation runtime

## Status

Accepted.

## Context

The engine evaluates on whatever executor the embedder uses. The C FFI evaluates inside
`block_on` on many caller threads (cgo calls from many Go threads), batch evaluation uses a pool
of thread-local runtimes, and Rust embedders bring their own. Two things follow from that:

- sqlx sockets are registered with the reactor of the runtime they were opened on. A pool opened
  on a runtime that is not being driven stops working.
- On a current-thread tokio runtime only one caller at a time can hold the scheduler core inside
  `block_on`, so concurrent evaluations queue behind each other.

## Decision

- Database connection pools (`ms/database`, `databases.rs`) live on a dedicated runtime that is always
  driven. Queries are spawned onto it and the caller awaits the `JoinHandle`, which is
  executor-agnostic. Pools are created once per engine.
- The runtime used by every C-API evaluation (`bindings/c/src/mt.rs`) is a multi-thread runtime
  with one worker. On a multi-thread runtime `block_on` polls the future on the calling thread,
  so evaluations run in parallel. The single worker drives the shared timer and I/O drivers
  (function-node sleeps, HTTP) and runs anything an evaluation spawns.
- The runtime is returned by reference. Cloning an `Arc` per call would make every evaluation
  bump one shared counter, a cache line all threads would contend for.

## Consequences

- Evaluation works with any embedder executor, and concurrent C callers do not serialise.
- Database I/O is not tied to the lifetime or driving of the caller's runtime.
- Work spawned by an evaluation shares one worker thread.
