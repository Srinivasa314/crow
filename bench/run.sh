#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/bench/bin"
CROWC="$ROOT/target/release/crowc"
export GOCACHE="$BIN/go-cache"

mkdir -p "$BIN"

command -v go >/dev/null || {
  echo "go is required for comparison" >&2
  exit 1
}
command -v hyperfine >/dev/null || {
  echo "hyperfine is required for timing" >&2
  exit 1
}

echo "building crowc and Crow runtime..."
cargo build --release -p crowc -p crow-runtime --manifest-path "$ROOT/Cargo.toml"

benchmarks=(fib mandelbrot binary_trees)

for name in "${benchmarks[@]}"; do
  echo "compiling $name..."
  "$CROWC" build "$ROOT/bench/crow/$name.crow" -o "$BIN/$name-crow"
  go build -o "$BIN/$name-go" "$ROOT/bench/go/$name.go"

  crow_out="$("$BIN/$name-crow")"
  go_out="$("$BIN/$name-go")"
  if [[ "$crow_out" != "$go_out" ]]; then
    echo "$name output mismatch" >&2
    echo "crow: $crow_out" >&2
    echo "go:   $go_out" >&2
    exit 1
  fi
done

for name in "${benchmarks[@]}"; do
  echo
  echo "== $name =="
  hyperfine --warmup 3 --runs 10 \
    --command-name "crow $name" "$BIN/$name-crow" \
    --command-name "go $name" "$BIN/$name-go"
done
