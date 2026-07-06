#!/usr/bin/env bash
set -e

GUEST_ONLY=false
if [[ "${1:-}" == "guest" ]]; then
    GUEST_ONLY=true
fi

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
cargo run -p compiler -- guest.component.wasm host/src/guest.pulley

if $GUEST_ONLY; then
    echo
    echo "========================================"
    echo "✅ Guest build completed successfully"
    echo "========================================"
    exit 0
fi

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
