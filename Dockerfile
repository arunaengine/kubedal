FROM rust:1.84-slim as builder

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
COPY --from=builder /workspace/target/release/k8s-kubedal-csi /usr/local/bin/k8s-kubedal-csi

# Set the entrypoint
ENTRYPOINT ["k8s-kubedal-csi"]