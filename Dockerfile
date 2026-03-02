FROM --platform=linux/amd64 rust:1.85.0-bookworm AS builder

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
COPY Cargo.toml Cargo.lock ./
COPY crates crates

RUN cargo build --release --bin sn2-validator --bin sn2-miner

FROM --platform=linux/amd64 debian:bookworm-20250224-slim

RUN apt-get update && apt-get install -y \
    jq \
    aria2 \
    curl \
    ca-certificates \
    gosu \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash subnet2

ENV NVM_DIR=/opt/.nvm
RUN mkdir -p /opt/.nvm /opt/.snarkjs && \
    chown -R subnet2:subnet2 /opt/.nvm /opt/.snarkjs

USER subnet2
RUN curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.0/install.sh | bash && \
    export NVM_DIR="$NVM_DIR" && \
    [ -s "$NVM_DIR/nvm.sh" ] && \. "$NVM_DIR/nvm.sh" && \
    nvm install 20 && \
    nvm use 20 && \
    npm install --prefix /opt/.snarkjs snarkjs@0.7.4 && \
    mkdir -p ~/.local/bin && \
    ln -s "$NVM_DIR/versions/node/$(nvm version)/bin/node" /home/subnet2/.local/bin/node && \
    ln -s "$NVM_DIR/versions/node/$(nvm version)/bin/npm" /home/subnet2/.local/bin/npm && \
    ln -s /opt/.snarkjs/node_modules/.bin/snarkjs /home/subnet2/.local/bin/snarkjs
ENV PATH="/home/subnet2/.local/bin:${PATH}"

USER root

COPY --from=builder /build/target/release/sn2-validator /usr/local/bin/sn2-validator
COPY --from=builder /build/target/release/sn2-miner /usr/local/bin/sn2-miner

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

EXPOSE 8091/tcp
EXPOSE 8443/tcp
EXPOSE 9090/tcp
