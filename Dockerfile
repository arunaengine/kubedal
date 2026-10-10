FROM rust:1.97-alpine3.23 AS builder
WORKDIR /build
RUN apk add --no-cache build-base cmake clang perl linux-headers protobuf-dev protoc git
# Link against the musl libc of the runtime image instead of a static binary.
ENV RUSTFLAGS="-C target-feature=-crt-static"
COPY . .
RUN cargo build --release --locked --bin kubedal

FROM alpine:3.23
RUN apk add --no-cache libgcc ca-certificates fuse3 util-linux
COPY --from=builder /build/target/release/kubedal /usr/local/bin/kubedal
ENTRYPOINT ["kubedal"]
