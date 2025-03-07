FROM rust:1.85-alpine3.21 AS builder
WORKDIR /build
RUN apk update
RUN apk upgrade
ENV RUSTFLAGS="-C target-feature=-crt-static"
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN apk add llvm cmake gcc ca-certificates libc-dev pkgconfig openssl-dev protoc protobuf-dev protobuf-dev musl-dev git curl openssh
COPY . .
RUN cargo build --release

FROM alpine:3.21
RUN apk update
RUN apk upgrade
RUN apk add libgcc gcompat ca-certificates openssl-dev fuse3
COPY --from=builder /build/target/release/kubedal /usr/local/bin/kubedal
ENTRYPOINT ["kubedal"]