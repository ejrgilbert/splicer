#!/usr/bin/env bash
# End-to-end proof for the resource-method-interception design
# (see docs/TODO/resource-method-interception.md).
# Builds realprov -> wrapper -> edge, composes with wac, runs with wasmtime.
set -euo pipefail
cd "$(dirname "$0")"

for c in realprov wrapper edge; do (cd "$c" && cargo component build --release >/dev/null); done

P=realprov/target/wasm32-wasip1/release/realprov.wasm
W=wrapper/target/wasm32-wasip1/release/wrapper.wasm
E=edge/target/wasm32-wasip1/release/edge.wasm

wac compose --dep host:kv="$P" --dep splice:wrap="$W" --dep splice:edge="$E" compose.wac -o full.wasm
wasm-tools validate full.wasm
OUT=$(wasmtime run --invoke 'run()' full.wasm)
EXPECT='"via_t=Some(\"fromraw\") via_raw=Some(\"fromraw\")"'
echo "result:   $OUT"
echo "expected: $EXPECT"
if [ "$OUT" = "$EXPECT" ]; then echo "PASS"; else echo "FAIL"; exit 1; fi
