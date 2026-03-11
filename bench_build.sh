#!/bin/bash
# Build script for vsock throughput benchmark enclave
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

DOCKER_IMAGE="bench-enclave:v1"
EIF_OUTPUT="bench-enclave.eif"

echo "=============================================="
echo "Building Benchmark Enclave"
echo "=============================================="

echo ""
echo "[1/2] Building Docker image..."
docker build --platform linux/amd64 -t "$DOCKER_IMAGE" ./bench-enclave/

echo ""
echo "[2/2] Building Enclave Image File (EIF)..."
nitro-cli build-enclave \
    --docker-uri "$DOCKER_IMAGE" \
    --output-file "$EIF_OUTPUT"

echo ""
echo "=============================================="
echo "Build Complete: $EIF_OUTPUT"
echo "=============================================="
