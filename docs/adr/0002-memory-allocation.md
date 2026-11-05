# 0002. Allocator, purging and the memory shared with the C caller

## Status

Accepted.

## Context

libzen_ffi is called from many threads (cgo calls, plus the database runtime's threads) and
returns large responses (tens of megabytes of JSON from large database results).

- glibc malloc, the upstream allocator, keeps a separate arena per thread and returns little of
  it to the OS after a burst. A process stays near its peak memory long after the load ends.
- mimalloc returns freed pages to the OS after a short delay and is faster for the many small
  allocations of an evaluation. It purges lazily, from inside later allocator calls. After a
  burst the threads that did the work sit idle, so nothing would run the purge.
- mimalloc v3 madvises its arenas `MADV_HUGEPAGE` by default. With transparent huge pages it
  purges only whole 2 MiB ranges, and the kernel keeps a partly freed huge page charged until it
  splits it, so freed memory stays resident.
- glibc raises its mmap threshold (up to 32 MiB) each time a large mmapped block is freed, and
  the trim threshold with it. Response buffers would then come from per-thread arenas that are
  never trimmed.
- The C contract is that memory the library hands to its caller is freed by the caller with
  `free()`, and memory the caller hands over (loader and custom-node callback results) was
  allocated with `malloc()`.

## Decision

- Rust code in libzen_ffi allocates with mimalloc (`ms_alloc.rs`).
- One background thread (`start_purger`, started when an engine is created) calls
  `mi_collect(false)` once a second. That purges every arena range whose delay has expired,
  whichever thread freed it, plus that thread's own heap. Idle memory returns within about a
  second. The purge delay on the evaluation path stays at mimalloc's default. Purging on the
  evaluation path (`MIMALLOC_PURGE_DELAY=0`, or collecting after each large evaluation) was
  rejected because it puts the purge cost on every evaluation.
- mimalloc is built without transparent huge pages (the crate's `no_thp` feature).
- On glibc Linux, `start_purger` fixes the mmap threshold at 1 MiB with `mallopt`. This turns off
  glibc's dynamic adjustment. Every response of 1 MiB or more is its own mapping, returned to the
  OS by the caller's `free()` and grown by `mremap` without copying.
- Buffers crossing the C boundary are allocated and released with libc `malloc` / `free`, never
  with the Rust global allocator.

## Consequences

- Memory falls back after a burst of large evaluations without a cost on the evaluation path.
- If the purge thread cannot be spawned, memory is still freed, only returned to the OS later.
- Buffers crossing the C boundary must go through the helpers in `ms_alloc.rs`.
