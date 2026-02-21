# Multi-stage build for reproducible enclave image
# Base image pinned by digest for reproducibility (amd64 linux)
FROM --platform=linux/amd64 rust:1.82.0-slim@sha256:2893c948181a4f145098f8461ba4dfc61d5b85e7f3c46d18dddc099f0d73217c AS builder

# Set environment for reproducible builds
ENV SOURCE_DATE_EPOCH=0
ENV CARGO_HOME=/cargo
ENV RUSTFLAGS="-C target-feature=+crt-static"

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-tools \
    && rm -rf /var/lib/apt/lists/*

# Add musl target for static linking
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# Copy manifests first for better caching
COPY Cargo.toml Cargo.lock ./

# Create dummy src to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Build dependencies only (for caching)
RUN cargo build --release --target x86_64-unknown-linux-musl --locked || true

# Remove dummy source
RUN rm -rf src

# Copy actual source
COPY src ./src

# Touch main.rs to ensure rebuild
RUN touch src/main.rs

# Build the actual application
RUN cargo build --release --target x86_64-unknown-linux-musl --locked

# Runtime stage - minimal image
FROM --platform=linux/amd64 scratch

# Copy only the static binary
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/enclave-app /enclave-app

# Set entrypoint
ENTRYPOINT ["/enclave-app"]
