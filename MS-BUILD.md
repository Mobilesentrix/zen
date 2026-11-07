# MS build & test — zen 2.1.6 + database node

All commands run from `engine-v2/zen` in Git Bash. Cargo runs inside `rust:1-bookworm`
(linux/amd64) through `ms/scripts/docker-cargo.sh`, which mounts the workspace at `/work` and
keeps the cargo registry and target dir in the named Docker volumes `ms-zen-cargo-registry`,
`ms-zen-cargo-git`, `ms-zen-target` (the Windows-GNU host toolchain cannot link from a path
containing a space, so host builds are not used).

## Layout

| path | what |
|---|---|
| `ms/database/` | crate `ms-zen-database`: `CustomNodeAdapter` for kind `database`, placeholder parser/binder, pools, row decoding |
| `bindings/c/src/ms_database.rs` | new C constructors (see MS-FFI.md) |
| `bindings/c/src/ms_cache.rs` | `zen_engine_invalidate_decision` / `zen_engine_clear_decisions` (MS-FFI.md §1 "Decision cache") |
| `bindings/c/src/{custom_node,error,lib}.rs`, `languages/go.rs` | 1–6 line hooks: `Database` adapter variant, error code 12, module wiring, `map_handler` made `pub(crate)` |
| `bindings/c/src/{engine,loader}.rs` | callback loaders wrapped in `CachedLoader`; `DynamicDecisionLoader` delegates `keys`/`load_sync` so `compile()` works for loader-config engines |
| `core/engine/src/loader/cached.rs` | additive `CachedLoader::{invalidate, clear}` |
| `core/engine/src/nodes/failure.rs` (+ 1–40 line hooks in `decision_graph/{tracer,walker}.rs`, `nodes/{context,expression,decision,transform_attributes}.rs`) | error trace: `error` on the failing node's trace entry (MS-FFI.md "Error trace") |
| `Cargo.toml` | workspace member `ms/database` |
| `ms-test/` | docker compose MySQL 8.4 + Postgres 16 with fixtures |

## Build libzen_ffi.a (linux_amd64)

```bash
ms/scripts/docker-cargo.sh build --release -p zen-ffi
mkdir -p ../out/libzen_ffi/linux_amd64
MSYS_NO_PATHCONV=1 docker run --rm -v ms-zen-target:/target -v "$(pwd -W)/../out:/out" rust:1-bookworm \
  sh -c 'cp /target/release/libzen_ffi.a /out/libzen_ffi/linux_amd64/ && strip --strip-debug /out/libzen_ffi/linux_amd64/libzen_ffi.a'
cp bindings/c/zen_engine.h ../out/libzen_ffi/linux_amd64/
```

### linux_arm64 (cross-compiled from the same amd64 container)

```bash
MSYS_NO_PATHCONV=1 docker run --rm --platform linux/amd64 -v "$(pwd -W):/work" \
  -v ms-zen-cargo-registry:/usr/local/cargo/registry -v ms-zen-cargo-git:/usr/local/cargo/git \
  -v ms-zen-target-arm64:/target -e CARGO_TARGET_DIR=/target \
  -e CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc -e CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
  -e AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc -w /work rust:1-bookworm \
  sh -c 'apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu g++-aarch64-linux-gnu binutils-aarch64-linux-gnu \
         && rustup target add aarch64-unknown-linux-gnu \
         && cargo build --release -p zen-ffi --target aarch64-unknown-linux-gnu \
         && aarch64-linux-gnu-strip --strip-debug /target/aarch64-unknown-linux-gnu/release/libzen_ffi.a'
mkdir -p ../out/libzen_ffi/linux_arm64
MSYS_NO_PATHCONV=1 docker run --rm -v ms-zen-target-arm64:/target -v "$(pwd -W)/../out:/out" rust:1-bookworm \
  cp /target/aarch64-unknown-linux-gnu/release/libzen_ffi.a /out/libzen_ffi/linux_arm64/
cp bindings/c/zen_engine.h ../out/libzen_ffi/linux_arm64/
```

(`ring`, `aws-lc-sys`, `rquickjs-sys` and `mimalloc` compile their C/asm with the cross gcc;
the host `cc` is only used for build scripts. No qemu needed.)

`build.rs` regenerates `bindings/c/zen_engine.h` (cbindgen) on every build.
Release profile is upstream's (`lto = true`, `codegen-units = 1`, `strip = "symbols"`).

Native libs to link (from `cargo rustc --print native-static-libs`): see MS-FFI.md §5.

## Unit tests (no database)

```bash
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p ms-zen-database
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-ffi --no-default-features
MS_TARGET_VOLUME=ms-zen-target-test ms/scripts/docker-cargo.sh test -p zen-engine --features arbitrary_precision
```

`--features arbitrary_precision` is required for `zen-engine`: it is the feature set the shipped lib
uses (`bindings/c` enables it), and upstream's tests are written for it. All 134 snapshots under
`core/engine/tests/snapshots/` were recorded with it (numbers appear as `$serde_json::private::Number`
strings), and the graph test data carry 28-digit decimals. Without the feature, `engine_graph_tests`
and `engine_snapshot_tests` fail: numbers go through f64 (`3.333333333333334` against
`3.3333333333333333333333333336`), and the snapshots show plain numbers. That is a test invocation
error, not an engine regression. Upstream CI never sees it, because `cargo test --workspace` unifies
features and `zen-uniffi` (not excluded there) turns arbitrary_precision on even in its "without
arbitrary_precision" step. Do not re-record snapshots from a featureless run.

`--no-default-features` is required for `zen-ffi`: its default `go` feature references the two
Go callback symbols, which only exist when cgo links the lib (the test binary cannot link
otherwise). The separate `ms-zen-target-test` volume keeps debug test builds out of the
release target dir used for the shipped lib.

## Integration tests (MySQL + Postgres containers)

```bash
(cd ms-test && docker compose up -d --wait)
MS_DOCKER_NETWORK=ms-zen-test \
MS_TEST_MYSQL_URL='mysql://root:test@ms-zen-mysql:3306/ms_test' \
MS_TEST_PG_URL='postgres://postgres:test@ms-zen-pg:5432/ms_test' \
MS_CORPUS_HOST="$(cd ../../corpus && pwd -W)" \
  ms/scripts/docker-cargo.sh test --release -p ms-zen-database -- --include-ignored --nocapture
(cd ms-test && docker compose down -v)
```

`--include-ignored` runs the DB tests, the corpus test (reads the dev corpus export mounted
read-only at `/corpus`) and the timed benchmark (`bench_decision_with_and_without_db_node`,
prints `BENCH` lines).

### Against the corpus test databases (engine-v2/testdb: StarRocks 3.5 + Postgres 16)

`tests/testdb.rs` runs the `resultHandling` cases against the seeded StarRocks
(`ms_analytics.catalog_product`, duplicate SKU `107011215481`) and Postgres (`samplePg.orders`):

```bash
docker compose -f ../testdb/docker-compose.yml up -d --wait
PGPW="$(grep '^POSTGRES_PASSWORD=' ../testdb/.env.docker | cut -d= -f2-)"
MS_DOCKER_NETWORK=ms-ruleengine-testdb MS_TARGET_VOLUME=ms-zen-target-test \
MS_TESTDB_STARROCKS_URL='starrocks://root@starrocks:9030/mobilesentrix_dev' \
MS_TESTDB_PG_URL="postgres://testdb:${PGPW}@testdb-postgres:5432/samplePg" \
  ms/scripts/docker-cargo.sh test -p ms-zen-database --test testdb -- --include-ignored
```

### StarRocks prepared statements (any StarRocks FE)

`tests/starrocks_prepared.rs` creates its own fixture (`ms_zen_stale_params`) and checks that a
Lookup with a correlated subquery returns the row asked for on one reused connection, that the
server bug it works around is still there (`upstream_starrocks_reexecuted_statement_keeps_first_parameters`),
and prints the per-query cost of re-preparing (`PREPARE_COST` lines). See
`docs/starrocks-prepared-statement-stale-params.md`.

```bash
docker network create ms-zen-sr
docker run -d --name ms-zen-starrocks --network ms-zen-sr starrocks/allin1-ubuntu:4.1.4
# wait until `SHOW BACKENDS` reports Alive: true, then
MS_DOCKER_NETWORK=ms-zen-sr MS_TEST_STARROCKS_URL='mysql://root@ms-zen-starrocks:9030'   ms/scripts/docker-cargo.sh test -p ms-zen-database --test starrocks_prepared -- --include-ignored --nocapture
docker rm -f ms-zen-starrocks && docker network rm ms-zen-sr
```

## C ABI smoke test

`ms-test/ffi_smoke.c` links `libzen_ffi.a` with a C program and evaluates a decision with a
database node through the new constructor:

```bash
MSYS_NO_PATHCONV=1 docker run --rm --network ms-zen-test \
  -v "$(pwd -W):/work" -v "$(cd ../out && pwd -W):/out" -w /work rust:1-bookworm \
  sh -c 'cc -o /tmp/smoke ms-test/ffi_smoke.c -I/out/libzen_ffi/linux_amd64 \
         /out/libzen_ffi/linux_amd64/libzen_ffi.a -lm -ldl -lpthread && /tmp/smoke'
```

## Dependency audit (cargo audit)

```bash
MSYS_NO_PATHCONV=1 docker run --rm -v "$(pwd -W):/work:ro" -v ms-zen-cargo-registry:/usr/local/cargo/registry \
  -w /work rust:1-bookworm sh -c 'cargo install cargo-audit --locked -q; cargo audit --color never'
```

Compatible bumps go through `cargo update -p <crate>` on the host (no linking needed), then a
lib rebuild + parity. The advisories inside the `zen-ffi` tree that need an upstream change are `zip` 8.6 (RUSTSEC-2026-0367/0368/0369, fix is 9.0 — waits for upstream
zen), `quick-xml` (RUSTSEC-2026-0194/0195, fix is 0.41 — upstream) and `rsa`
(RUSTSEC-2023-0071, no fix available — accepted). quinn-proto, webpki-roots, rkyv and pyo3
findings belong to the Python/Node binding members only, not to `libzen_ffi.a`
(`cargo tree -p zen-ffi --no-default-features -i <crate>` is empty).
