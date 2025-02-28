FROM rust:1.85-slim as builder

WORKDIR /workspace

# Install build dependencies
RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    libfuse-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Add source files
COPY proto/ proto/
COPY Cargo.toml .
COPY build.rs .
COPY src/ src/

# Build release binary
RUN cargo build --release

# Runtime image
FROM debian:bullseye-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libfuse2 \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder stage
COPY --from=builder /workspace/target/release/kubedal /usr/local/bin/kubedal

# Set the entrypoint
ENTRYPOINT ["kubedal"]