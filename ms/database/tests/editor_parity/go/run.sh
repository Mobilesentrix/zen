#!/usr/bin/env bash
# LF line endings required
# Usage (inside golang:1.25): run.sh <go-module rulestore dir> <cases.jsonl> > expected.jsonl
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
cp "$here/main.go" "$work/"
for f in database_node.go where_clause.go sql_literal.go; do
  sed -e 's/^package rulestore/package main/' \
      -e '/zen "github.com\/Mobilesentrix\/ms-zen-go\/v2"/d' \
      -e 's/zen\.DatabaseNodeKind/"database"/g' "$1/$f" > "$work/$f"
done
cd "$work" && go mod init parity >/dev/null 2>&1 && go run . "$2"
