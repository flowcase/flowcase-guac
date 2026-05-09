# --- builder ---
FROM rust:1-slim AS builder

# aws-lc-rs (rustls's default crypto provider) needs a C toolchain
# and cmake to build its FFI bits.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        cmake \
        build-essential \
        pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --quiet

# --- runtime ---
FROM ubuntu:24.04

RUN apt-get update && \
    apt-get install -y --no-install-recommends guacd ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /opt/flowcase-guac

COPY --from=builder /build/target/release/flowcase_guac /usr/local/bin/flowcase-guac
COPY public /opt/flowcase-guac/public
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV FLOWCASE_GUAC_PUBLIC_DIR=/opt/flowcase-guac/public
ENV FLOWCASE_GUACD_ADDR=127.0.0.1:4822
ENV GUAC_KEY=secret

EXPOSE 8080

ENTRYPOINT ["docker-entrypoint.sh"]
