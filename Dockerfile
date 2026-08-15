FROM rust:1.88-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang \
        gcc \
        libelf-dev \
        linux-libc-dev \
        llvm \
        make \
        pkg-config \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --locked --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libelf1 zlib1g \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/rustprofile /usr/local/bin/rustprofile
ENTRYPOINT ["/usr/local/bin/rustprofile"]
