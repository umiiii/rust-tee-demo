#!/bin/bash
# Build script for Nitro Enclave verifiable computation demo
# This script builds the enclave image and generates docker_image_info.json

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Configuration
DOCKER_IMAGE="enclave-app:v1"
EIF_OUTPUT="enclave.eif"
DOCKER_INFO_OUTPUT="docker_image_info.json"

echo "=============================================="
echo "Building Nitro Enclave Application"
echo "=============================================="

# Step 1: Build Docker image
echo ""
echo "[1/4] Building Docker image..."
docker build --platform linux/amd64 -t "$DOCKER_IMAGE" ./enclave-app/

echo ""
echo "[2/4] Building Enclave Image File (EIF)..."
# Build EIF and capture output
EIF_BUILD_OUTPUT=$(nitro-cli build-enclave \
    --docker-uri "$DOCKER_IMAGE" \
    --output-file "$EIF_OUTPUT" 2>&1)

echo "$EIF_BUILD_OUTPUT"

# Extract PCR values from output
echo ""
echo "[3/4] Extracting PCR values..."
PCR0=$(echo "$EIF_BUILD_OUTPUT" | grep -o '"PCR0": "[^"]*"' | cut -d'"' -f4 || echo "")
PCR1=$(echo "$EIF_BUILD_OUTPUT" | grep -o '"PCR1": "[^"]*"' | cut -d'"' -f4 || echo "")
PCR2=$(echo "$EIF_BUILD_OUTPUT" | grep -o '"PCR2": "[^"]*"' | cut -d'"' -f4 || echo "")

if [ -z "$PCR0" ]; then
    echo "Warning: Could not extract PCR0 from build output"
    PCR0="<extraction failed>"
fi

echo "PCR0: $PCR0"
echo "PCR1: $PCR1"
echo "PCR2: $PCR2"

# Step 4: Generate docker_image_info.json
echo ""
echo "[4/4] Generating docker_image_info.json..."

# Read Dockerfile content
DOCKERFILE_CONTENT=$(cat ./enclave-app/Dockerfile | jq -Rs .)

# Get git commit
GIT_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "not-a-git-repo")

# Create JSON
cat > "$DOCKER_INFO_OUTPUT" << EOF
{
    "dockerfile_content": $DOCKERFILE_CONTENT,
    "git_commit": "$GIT_COMMIT",
    "build_command": "docker build --platform linux/amd64 -t $DOCKER_IMAGE ./enclave-app/ && nitro-cli build-enclave --docker-uri $DOCKER_IMAGE --output-file $EIF_OUTPUT",
    "pcr0": "$PCR0",
    "pcr1": "$PCR1",
    "pcr2": "$PCR2"
}
EOF

echo "Generated: $DOCKER_INFO_OUTPUT"

echo ""
echo "=============================================="
echo "Build Complete!"
echo "=============================================="
echo ""
echo "Outputs:"
echo "  - EIF file: $EIF_OUTPUT"
echo "  - Docker info: $DOCKER_INFO_OUTPUT"
echo ""
echo "PCR Values (for verification):"
echo "  PCR0: $PCR0"
echo "  PCR1: $PCR1"
echo "  PCR2: $PCR2"
echo ""
echo "Next step: Run the demo with ./run_demo.sh"
