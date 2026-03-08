# ── Stage 1: Shared builder (compiles all workspace binaries) ─────────────
FROM rust:1.90 AS builder

WORKDIR /app

# Install protobuf compiler (required by tonic-build)
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev && rm -rf /var/lib/apt/lists/*

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
RUN cargo build --release --bin social-api --bin mock-services 2>/dev/null || true

# Copy actual source code and migrations (social-api embeds migrations via sqlx)
COPY crates/ crates/
COPY migrations/ migrations/

# Touch source files to invalidate cache for source changes
RUN touch crates/social-api/src/main.rs crates/social-api/src/lib.rs \
          crates/shared/src/lib.rs crates/mock-services/src/main.rs

# Build both release binaries (profile.release in Cargo.toml: lto=fat, codegen-units=1, strip)
RUN cargo build --release --bin social-api --bin mock-services

# ── Stage 2a: social-api runtime ─────────────────────────────────────────
FROM gcr.io/distroless/cc-debian13:nonroot AS social-api

COPY --from=builder /app/target/release/social-api /usr/local/bin/social-api

EXPOSE 8080 50051

ENTRYPOINT ["social-api"]

# ── Stage 2b: mock-services runtime ──────────────────────────────────────
FROM gcr.io/distroless/cc-debian13:nonroot AS mock-services

COPY --from=builder /app/target/release/mock-services /usr/local/bin/mock-services

EXPOSE 8081 50052

ENTRYPOINT ["mock-services"]
