#!/usr/bin/env bash
# LF line endings required
# Runs cargo inside a linux/amd64 Rust container against this workspace.
# Usage: ms/scripts/docker-cargo.sh <cargo args...>
#   e.g. ms/scripts/docker-cargo.sh test -p ms-zen-database
# Env:
#   MS_DOCKER_NETWORK  docker network to join (for integration tests), default: bridge
#   MS_RUST_IMAGE      default: rust:1-bookworm
#   MS_TARGET_VOLUME   docker volume for CARGO_TARGET_DIR, default: ms-zen-target
#   MS_CORPUS_HOST    host path of the dev corpus export; mounted read-only at /corpus
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && (pwd -W 2>/dev/null || pwd))"
image="${MS_RUST_IMAGE:-rust:1-bookworm}"
network="${MS_DOCKER_NETWORK:-bridge}"

env_args=()
for v in MS_TEST_MYSQL_URL MS_TEST_PG_URL MS_TESTDB_STARROCKS_URL MS_TESTDB_PG_URL RUST_BACKTRACE \
         MS_LISTING_URL MS_LISTING_SCHEMA MS_LISTING_RULES MS_LISTING_TBA MS_LISTING_ROUNDS MS_LISTING_ITER \
         MS_LISTING_CASES MS_LISTING_SECONDS MS_LISTING_WORKERS MS_LISTING_RATE MS_LISTING_VARIANTS MS_LISTING_LOAD_SHAPES; do
  if [ -n "${!v:-}" ]; then env_args+=(-e "$v=${!v}"); fi
done
if [ -n "${MS_CORPUS_HOST:-}" ]; then
  env_args+=(-v "$MS_CORPUS_HOST:/corpus:ro" -e MS_CORPUS_DIR=/corpus)
fi

MSYS_NO_PATHCONV=1 docker run --rm \
  --platform linux/amd64 \
  --network "$network" \
  -v "$here:/work" \
  -v ms-zen-cargo-registry:/usr/local/cargo/registry \
  -v ms-zen-cargo-git:/usr/local/cargo/git \
  -v "${MS_TARGET_VOLUME:-ms-zen-target}:/target" \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_BUILD_JOBS="$(nproc 2>/dev/null || echo 8)" \
  "${env_args[@]}" \
  -w /work \
  "$image" \
  cargo "$@"
