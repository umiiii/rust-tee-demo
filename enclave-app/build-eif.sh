#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cd "$SCRIPT_DIR"

TARGET=x86_64-unknown-linux-musl
cargo build --release --features vsock --target "$TARGET"

DOCKER_IMAGE=enclave:latest
EIF_OUTPUT=enclave.eif

cp "target/$TARGET/release/enclave-app" enclave

docker build -t $DOCKER_IMAGE .

nitro-cli build-enclave \
    --docker-uri "$DOCKER_IMAGE" \
    --output-file "$EIF_OUTPUT"