FROM rust:1.88-bullseye AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        clang-13 \
        gcc \
        libelf-dev \
        linux-libc-dev \
        llvm-13 \
        make \
        pkg-config \
        zlib1g-dev \
    && ln -s /usr/bin/clang-13 /usr/local/bin/clang \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --locked --release

FROM debian:bullseye-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libelf1 zlib1g \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/rustprofile /usr/local/bin/rustprofile
ENTRYPOINT ["/usr/local/bin/rustprofile"]
