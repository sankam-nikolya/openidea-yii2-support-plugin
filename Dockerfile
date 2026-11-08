# Build OpenIDEA Yii2 Support plugin (wasm32-wasip2 component).
#
# SDK (wit-parser / wasmparser 0.262) requires rustc >= 1.88.
#
# Usage:
#   ./build.sh docker
#   # or:
#   docker build --build-arg UID=$(id -u) --build-arg GID=$(id -g) -t yii2support-plugin .
#   docker run --rm -v "$PWD/dist:/out" yii2support-plugin

FROM rust:1.88 AS builder

ARG UID=1000
ARG GID=1000

RUN rustup target add wasm32-wasip2 \
    && apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates zip \
    && rm -rf /var/lib/apt/lists/* \
    && rustc --version

WORKDIR /plugin

COPY Cargo.toml ./
RUN mkdir -p src && printf '%s\n' '//! stub' 'pub fn _stub() {}' > src/lib.rs \
    && cargo fetch --target wasm32-wasip2 || true

COPY src ./src
COPY plugin.toml settings.json ./

RUN cargo build --release --target wasm32-wasip2

# Produce dist artifacts owned by the host user (UID/GID build-args).
RUN mkdir -p /out \
    && WASM=$(find target/wasm32-wasip2/release -maxdepth 1 -name '*.wasm' ! -name '*deps*' | head -n1) \
    && test -n "$WASM" \
    && cp "$WASM" /out/plugin.wasm \
    && rustc --version > /out/rustc.version \
    && chown -R "${UID}:${GID}" /out \
    && ls -la /out

FROM debian:bookworm-slim AS export
ARG UID=1000
ARG GID=1000
ENV UID=${UID} GID=${GID}
COPY --from=builder /out/plugin.wasm /plugin.wasm
# Host ownership: build-arg UID/GID or docker run -e UID=$(id -u) -e GID=$(id -g)
CMD ["sh", "-c", "mkdir -p /out && cp /plugin.wasm /out/plugin.wasm && chown ${UID}:${GID} /out/plugin.wasm && ls -la /out/plugin.wasm"]
