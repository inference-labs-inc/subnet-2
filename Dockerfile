ARG SN2_PLATFORM=linux/amd64
FROM --platform=$SN2_PLATFORM rust:1.95.0-bookworm@sha256:503651ea31e66ecb74623beabde781059a5978df1595a9e8ed03974d5fec1bf0 AS chef

RUN cargo install cargo-chef --locked
RUN apt-get update && apt-get install -y \
    clang \
    llvm \
    pkg-config \
    libssl-dev \
    libudev-dev \
    protobuf-compiler \
    python3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY rust-toolchain.toml ./
RUN rustup show

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates crates

ARG SN2_VERSION=""
RUN CARGO_VERSION="${SN2_VERSION#v}" && \
    if echo "${CARGO_VERSION}" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+'; then \
      for f in crates/*/Cargo.toml; do \
        sed -i "s/^version\.workspace = true$/version = \"${CARGO_VERSION}\"/" "$f"; \
      done && \
      cargo update -w; \
    fi && \
    cargo build --release --locked --bin sn2-validator --bin sn2-miner

ARG SN2_PLATFORM=linux/amd64
FROM --platform=$SN2_PLATFORM debian:bookworm-20260518-slim@sha256:0104b334637a5f19aa9c983a91b54c89887c0984081f2068983107a6f6c21eeb AS runtime

RUN apt-get update && apt-get upgrade -y && apt-get install -y \
    jq \
    aria2 \
    curl \
    ca-certificates \
    gosu \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash subnet2

# Entrypoint elevates briefly to apply PUID remap, then execs `gosu subnet2`.
# Override the entrypoint at your own risk; default invocation drops privileges.
# nosemgrep: dockerfile.security.last-user-is-root.last-user-is-root
USER root

RUN cat <<'EOF' > /entrypoint.sh
#!/usr/bin/env bash
set -e

cmd="$1"
case "$cmd" in
    miner.py)     echo "Remapping miner.py -> sn2-miner" >&2; shift; set -- sn2-miner "$@" ;;
    validator.py) echo "Remapping validator.py -> sn2-validator" >&2; shift; set -- sn2-validator "$@" ;;
esac

if [ -n "$PUID" ]; then
    if [ "$PUID" = "0" ]; then
        echo "PUID=0 (root) is not permitted; running as subnet2" >&2
        exec gosu subnet2 "$@"
    elif ! echo "$PUID" | grep -qE '^[0-9]+$'; then
        echo "PUID=$PUID is not a valid numeric UID; running as subnet2" >&2
        exec gosu subnet2 "$@"
    else
        usermod -u "$PUID" subnet2
        exec gosu subnet2 "$@"
    fi
else
    exec gosu subnet2 "$@"
fi
EOF
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
CMD ["sn2-validator", "--help"]

EXPOSE 8091/udp
EXPOSE 8443/tcp
EXPOSE 9090/tcp

FROM runtime AS release
COPY sn2-validator /usr/local/bin/sn2-validator
COPY sn2-miner /usr/local/bin/sn2-miner
RUN chmod +x /usr/local/bin/sn2-validator /usr/local/bin/sn2-miner

FROM runtime AS dev
COPY --from=builder /build/target/release/sn2-validator /usr/local/bin/sn2-validator
COPY --from=builder /build/target/release/sn2-miner /usr/local/bin/sn2-miner
