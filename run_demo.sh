#!/bin/bash
# End-to-end demo script for Nitro Enclave verifiable computation
# This script runs the complete pipeline: build, run, verify

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Configuration
EIF_FILE="enclave.eif"
INPUT_FILE="test_input.json"
OUTPUT_DIR="./output"
DOCKER_INFO="docker_image_info.json"

echo "=============================================="
echo "Nitro Enclave Verifiable Computation Demo"
echo "=============================================="

# Check if we need to build
if [ ! -f "$EIF_FILE" ] || [ ! -f "$DOCKER_INFO" ]; then
    echo ""
    echo "[*] EIF or docker_image_info.json not found, running build..."
    ./build.sh
fi

# Build host-app if needed
if [ ! -f "./host-app/target/release/host-app" ]; then
    echo ""
    echo "[*] Building host-app..."
    cd host-app
    cargo build --release
    cd ..
fi

# Create output directory
mkdir -p "$OUTPUT_DIR"

echo ""
echo "=============================================="
echo "Running Enclave Computation"
echo "=============================================="
echo ""
echo "Input file: $INPUT_FILE"
echo "Input content:"
cat "$INPUT_FILE"
echo ""

# Run host app
./host-app/target/release/host-app "$EIF_FILE" "$INPUT_FILE" "$OUTPUT_DIR"

echo ""
echo "=============================================="
echo "Verifying Proof Package"
echo "=============================================="

# Extract PCR0 from docker_image_info.json
PCR0=$(jq -r '.pcr0' "$DOCKER_INFO")

# Setup Python virtual environment if needed
if [ ! -d "./verifier/venv" ]; then
    echo "[*] Setting up Python virtual environment..."
    python3 -m venv ./verifier/venv
    ./verifier/venv/bin/pip install -q -r ./verifier/requirements.txt
fi

echo ""
# Run verification
./verifier/venv/bin/python ./verifier/verify.py \
    "$OUTPUT_DIR/proof_package.json" \
    "$OUTPUT_DIR/input.json" \
    --expected-pcr0 "$PCR0"

echo ""
echo "=============================================="
echo "Demo Complete!"
echo "=============================================="
echo ""
echo "Generated files in $OUTPUT_DIR/:"
ls -la "$OUTPUT_DIR/"
echo ""
echo "To verify on another machine, copy these files:"
echo "  - $OUTPUT_DIR/proof_package.json"
echo "  - $OUTPUT_DIR/input.json"
echo "  - verifier/ directory"
echo ""
echo "Then run:"
echo "  python verify.py proof_package.json input.json --expected-pcr0 <PCR0>"
