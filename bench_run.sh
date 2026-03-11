#!/bin/bash
# Run vsock throughput benchmark
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

EIF_FILE="bench-enclave.eif"
HOST_BIN="./bench-host/target/release/bench-host"

# Build EIF if missing
if [ ! -f "$EIF_FILE" ]; then
    echo "[*] EIF not found, running bench_build.sh..."
    ./bench_build.sh
fi

# Build host binary if needed
if [ ! -f "$HOST_BIN" ]; then
    echo "[*] Building bench-host..."
    cd bench-host
    cargo build --release
    cd ..
fi

# Run benchmark, passing all CLI args through
exec "$HOST_BIN" "$EIF_FILE" "$@"
