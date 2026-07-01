#!/usr/bin/env bash
set -e

echo "========================================"
echo "🛠  Building guest crate (wasm32 target)"
echo "========================================"
cargo build -p guest --target wasm32-unknown-unknown --release

echo
echo "========================================"
echo "📦 Creating WASM component"
echo "========================================"
wasm-tools component new \
  target/wasm32-unknown-unknown/release/guest.wasm \
  -o guest.component.wasm

echo
echo "========================================"
echo "🧩 Running compiler (Pulley)"
echo "========================================"
# This expects your compiler crate to be in the workspace
cargo run -p compiler -- guest.component.wasm host/src/guest.pulley

echo
echo "========================================"
echo "🚀 Running Pico 2 firmware (release)"
echo "========================================"
# Enter the host directory so the target in .cargo/config.toml is applied!
cd host
cargo run --release
cd ..

echo
echo "========================================"
echo "✅ All steps completed successfully"
echo "========================================"
