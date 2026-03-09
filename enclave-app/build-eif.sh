#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cd "$SCRIPT_DIR"

cargo build --release --features vsock

DOCKER_IMAGE=enclave:latest
EIF_OUTPUT=enclave.eif

cp target/release/enclave-app enclave

docker build -t $DOCKER_IMAGE .

nitro-cli build-enclave \
    --docker-uri "$DOCKER_IMAGE" \
    --output-file "$EIF_OUTPUT"