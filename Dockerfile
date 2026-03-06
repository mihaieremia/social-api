# Stage 1: Build
FROM rust:1.90 AS builder

WORKDIR /app

# Install protobuf compiler (required by tonic-build)
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY crates/social-api/Cargo.toml crates/social-api/Cargo.toml
COPY crates/shared/Cargo.toml crates/shared/Cargo.toml
COPY crates/mock-services/Cargo.toml crates/mock-services/Cargo.toml

# Copy proto files and build.rs (needed by tonic-build at compile time)
COPY proto/ proto/
COPY crates/social-api/build.rs crates/social-api/build.rs
COPY crates/mock-services/build.rs crates/mock-services/build.rs

# Create dummy src files to build dependencies
RUN mkdir -p crates/social-api/src crates/shared/src crates/mock-services/src && \
    echo "fn main() {}" > crates/social-api/src/main.rs && \
    echo "" > crates/social-api/src/lib.rs && \
    echo "" > crates/shared/src/lib.rs && \
    echo "fn main() {}" > crates/mock-services/src/main.rs

# Build dependencies only (cached layer)
RUN cargo build --release --bin social-api 2>/dev/null || true

# Copy actual source code
COPY crates/ crates/
COPY migrations/ migrations/

# Touch main files to invalidate cache for source changes
RUN touch crates/social-api/src/main.rs crates/social-api/src/lib.rs crates/shared/src/lib.rs

# Build the real binary (profile.release in Cargo.toml: lto=fat, codegen-units=1, strip)
RUN cargo build --release --bin social-api

# Stage 2: Runtime
FROM debian:trixie-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

# Non-root user
RUN useradd -r -s /bin/false -u 1001 appuser

COPY --from=builder /app/target/release/social-api /usr/local/bin/social-api

USER appuser

EXPOSE 8080 50051

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -f http://localhost:8080/health/live || exit 1

CMD ["social-api"]
