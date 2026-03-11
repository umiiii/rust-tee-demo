# Multi-stage build for benchmark enclave image
FROM --platform=linux/amd64 rust:1.82.0-slim@sha256:2893c948181a4f145098f8461ba4dfc61d5b85e7f3c46d18dddc099f0d73217c AS builder

ENV SOURCE_DATE_EPOCH=0
ENV CARGO_HOME=/cargo
ENV RUSTFLAGS="-C target-feature=+crt-static"

RUN sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list.d/debian.sources \
    && apt-get update && apt-get install -y --no-install-recommends \
    musl-tools \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

COPY Cargo.toml Cargo.lock ./

RUN mkdir src && echo "fn main() {}" > src/main.rs

RUN cargo build --release --target x86_64-unknown-linux-musl --locked || true

RUN rm -rf src

COPY src ./src

RUN touch src/main.rs

RUN cargo build --release --target x86_64-unknown-linux-musl --locked

FROM --platform=linux/amd64 scratch

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/bench-enclave /bench-enclave

ENTRYPOINT ["/bench-enclave"]
