#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

wasm_bindgen="${STRYK_WASM_BINDGEN:-wasm-bindgen}"
if [[ "$("$wasm_bindgen" --version)" != "wasm-bindgen 0.2.130" ]]; then
    echo "Stryk requires wasm-bindgen-cli 0.2.130." >&2
    exit 1
fi
args=(--locked -p stryk-web --release --target wasm32-unknown-unknown)
if [[ "${STRYK_LOCAL_WALLET:-}" == "1" ]]; then args+=(--features local-wallet); fi
cargo build "${args[@]}"
mkdir -p target/web/wasm
cp apps/web/static/index.html apps/web/static/stryk.css target/web/
"$wasm_bindgen" --target web --out-name stryk_web --out-dir target/web/wasm \
    target/wasm32-unknown-unknown/release/stryk-web.wasm
