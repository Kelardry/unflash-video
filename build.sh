#!/usr/bin/env bash
# Build the WebAssembly modules and their JS glue: the app's (web/pkg) and
# the built-in decoders' (web/pkg-dec: HEVC, VP9, VP8 and AV1, and the
# AC-3 / E-AC-3 and DTS sound decoders, loaded only for a file that needs
# one), and put the changelog next to the page.
set -euo pipefail
cd "$(dirname "$0")"
PROFILE="${1:-release}"
if [ "$PROFILE" = "release" ]; then
  cargo build -p unflash-wasm -p unflash-decoders --target wasm32-unknown-unknown --release
  OUT=target/wasm32-unknown-unknown/release
else
  cargo build -p unflash-wasm -p unflash-decoders --target wasm32-unknown-unknown
  OUT=target/wasm32-unknown-unknown/debug
fi
mkdir -p web/pkg web/pkg-dec
wasm-bindgen --target web --out-dir web/pkg --out-name unflash "$OUT/unflash_wasm.wasm"
wasm-bindgen --target web --out-dir web/pkg-dec --out-name unflash_decoders "$OUT/unflash_decoders.wasm"
if command -v wasm-opt >/dev/null 2>&1 && [ "$PROFILE" = "release" ]; then
  wasm-opt -O3 --enable-simd -o web/pkg/unflash_bg.wasm web/pkg/unflash_bg.wasm
  wasm-opt -O3 --enable-simd -o web/pkg-dec/unflash_decoders_bg.wasm web/pkg-dec/unflash_decoders_bg.wasm
fi
# the app's "What's new" reads the changelog from beside the page
cp CHANGELOG.md web/CHANGELOG.md
ls -la web/pkg web/pkg-dec
