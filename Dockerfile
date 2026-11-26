# syntax=docker/dockerfile:1
#
# docker build -t slskr .
# One binary with the web UI built in, on a distroless base. See docker-compose.yml.

ARG RUST_VERSION=1.100
ARG DEBIAN=trixie

# --- Web UI (WebAssembly, same on every platform) -----------------------------------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-${DEBIAN} AS ui
# Keep in step with flake.lock (nixpkgs) and the wasm-bindgen pin in Cargo.lock.
ARG TRUNK_VERSION=0.21.14
ARG WASM_BINDGEN_VERSION=0.2.130
ARG BINARYEN_VERSION=133
ARG BUILDARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    case "$BUILDARCH" in \
      amd64) arch=x86_64; bindgen_target=x86_64-unknown-linux-musl ;; \
      arm64) arch=aarch64; bindgen_target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported build platform $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    gh=https://github.com; \
    curl -fsSL "$gh/trunk-rs/trunk/releases/download/v$TRUNK_VERSION/trunk-$arch-unknown-linux-gnu.tar.gz" \
      | tar -xz -C /usr/local/bin trunk; \
    curl -fsSL "$gh/wasm-bindgen/wasm-bindgen/releases/download/$WASM_BINDGEN_VERSION/wasm-bindgen-$WASM_BINDGEN_VERSION-$bindgen_target.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 "wasm-bindgen-$WASM_BINDGEN_VERSION-$bindgen_target/wasm-bindgen"; \
    curl -fsSL "$gh/WebAssembly/binaryen/releases/download/version_$BINARYEN_VERSION/binaryen-version_$BINARYEN_VERSION-$arch-linux.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=2 "binaryen-version_$BINARYEN_VERSION/bin/wasm-opt"; \
    rustup target add wasm32-unknown-unknown
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=slskr-ui-target \
    cd crates/slskr-web && NO_COLOR=true trunk build --release --locked

# --- Daemon, with the UI embedded -----------------------------------------------------
FROM rust:${RUST_VERSION}-slim-${DEBIAN} AS daemon
WORKDIR /src
COPY . .
COPY --from=ui /src/dist /ui
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=slskr-daemon-target \
    SLSKR_EMBED_UI=/ui cargo build --release --locked -p slskr --features embed-ui \
    && install -D target/release/slskr /out/slskr \
    && mkdir -p /out/data

# --- Runtime ----------------------------------------------------------------------------
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=daemon /out/slskr /usr/local/bin/slskr
COPY --from=daemon --chown=10001:10001 /out/data /data
COPY LICENSE /usr/share/doc/slskr/
USER 10001:10001
# The container's network namespace is the boundary, so listen on every interface.
ENV SLSKR_WEB_LISTEN=0.0.0.0:5030
VOLUME /data
# 5030: web UI and API. 50300: incoming Soulseek peer connections (soulseek.listen_port).
EXPOSE 5030 50300
ENTRYPOINT ["/usr/local/bin/slskr", "--data-dir", "/data"]
