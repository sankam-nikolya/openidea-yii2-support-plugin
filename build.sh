#!/usr/bin/env bash
# Build Yii2 Support OpenIDEA plugin (local or Docker) and package for install.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

MODE="${1:-docker}"   # local | docker | check | package
OUT_DIR="${ROOT}/dist"
mkdir -p "$OUT_DIR"

HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

PLUGIN_ID="$(grep -E '^id\s*=' plugin.toml | head -1 | sed 's/.*=\s*"\(.*\)"/\1/')"
PLUGIN_VER="$(grep -E '^version\s*=' plugin.toml | head -1 | sed 's/.*=\s*"\(.*\)"/\1/')"
PKG_NAME="${PLUGIN_ID}-${PLUGIN_VER}.oiplugin"

copy_wasm() {
  local src="$1"
  if [[ ! -f "$src" ]]; then
    echo "error: wasm not found: $src" >&2
    return 1
  fi
  cp -f "$src" "${OUT_DIR}/plugin.wasm"
  # best-effort ownership when running as root in weird environments
  chown "${HOST_UID}:${HOST_GID}" "${OUT_DIR}/plugin.wasm" 2>/dev/null || true
  echo "→ ${OUT_DIR}/plugin.wasm ($(wc -c < "${OUT_DIR}/plugin.wasm") bytes)"
}

build_local() {
  if command -v openidea-plugin >/dev/null 2>&1; then
    echo "==> openidea-plugin build"
    openidea-plugin build
    chown "${HOST_UID}:${HOST_GID}" "${OUT_DIR}/plugin.wasm" 2>/dev/null || true
    return
  fi

  echo "==> cargo build --release --target wasm32-wasip2"
  if ! rustup target list --installed 2>/dev/null | grep -q 'wasm32-wasip2'; then
    rustup target add wasm32-wasip2
  fi
  cargo build --release --target wasm32-wasip2

  local wasm
  wasm="$(find target/wasm32-wasip2/release -maxdepth 1 -name '*.wasm' ! -path '*/deps/*' | head -n1 || true)"
  if [[ -z "${wasm}" ]]; then
    wasm="target/wasm32-wasip2/release/openidea_plugin_yii2_support.wasm"
  fi
  copy_wasm "$wasm"
}

build_docker() {
  if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker not found" >&2
    exit 1
  fi
  local image="yii2support-openidea-plugin:local"
  echo "==> docker build (${image}) UID=${HOST_UID} GID=${HOST_GID}"
  docker build \
    --build-arg "UID=${HOST_UID}" \
    --build-arg "GID=${HOST_GID}" \
    -t "${image}" \
    "${ROOT}"
  echo "==> docker run → ${OUT_DIR}"
  docker run --rm \
    -e "UID=${HOST_UID}" \
    -e "GID=${HOST_GID}" \
    -v "${OUT_DIR}:/out" \
    "${image}"
  if [[ ! -f "${OUT_DIR}/plugin.wasm" ]]; then
    echo "error: dist/plugin.wasm was not produced" >&2
    exit 1
  fi
  # Ensure host ownership even if container ignored args
  chown "${HOST_UID}:${HOST_GID}" "${OUT_DIR}/plugin.wasm" 2>/dev/null || true
  ls -la "${OUT_DIR}/plugin.wasm"
}

check_only() {
  if [[ ! -f "${OUT_DIR}/plugin.wasm" ]]; then
    echo "error: missing ${OUT_DIR}/plugin.wasm — run ./build.sh first" >&2
    exit 1
  fi
  if command -v openidea-plugin >/dev/null 2>&1; then
    echo "==> openidea-plugin check ."
    openidea-plugin check . || true
  else
    echo "note: openidea-plugin CLI not installed; skipping package validation"
  fi
  file "${OUT_DIR}/plugin.wasm" || true
  echo "ok: $(wc -c < "${OUT_DIR}/plugin.wasm") bytes"
}

# Zip layout for Install Plugin from Package File… / from Disk
# OpenIDEA accepts:
#   - folder with plugin.toml
#   - .oiplugin / .zip with package at root OR in its only folder
package_plugin() {
  if [[ ! -f "${OUT_DIR}/plugin.wasm" ]]; then
    echo "error: missing dist/plugin.wasm — build first (./build.sh docker)" >&2
    exit 1
  fi
  local stage pkg
  stage="$(mktemp -d)"
  pkg="${ROOT}/${PKG_NAME}"
  mkdir -p "${stage}/dist"
  cp -f plugin.toml settings.json "${stage}/"
  cp -f "${OUT_DIR}/plugin.wasm" "${stage}/dist/plugin.wasm"
  # optional readme for humans; not required by installer
  [[ -f README.md ]] && cp -f README.md "${stage}/" || true

  rm -f "${pkg}" "${OUT_DIR}/${PKG_NAME}"
  # package at zip root (plugin.toml at top level) — requires `zip`
  if ! command -v zip >/dev/null 2>&1; then
    echo "error: zip is required to build .oiplugin (install zip package)" >&2
    rm -rf "${stage}"
    exit 1
  fi
  (cd "${stage}" && zip -r -q "${pkg}" .)
  cp -f "${pkg}" "${OUT_DIR}/${PKG_NAME}"
  chown "${HOST_UID}:${HOST_GID}" "${pkg}" "${OUT_DIR}/${PKG_NAME}" 2>/dev/null || true
  rm -rf "${stage}"
  echo "→ ${pkg} ($(wc -c < "${pkg}") bytes)"
  echo "→ ${OUT_DIR}/${PKG_NAME}"
  echo
  echo "Install in OpenIDEA:"
  echo "  Settings › Plugins › gear › Install Plugin from Package File…"
  echo "  → select ${PKG_NAME}"
  echo "  or Install Plugin from Disk… → select this folder (with dist/plugin.wasm)"
  echo "  or: openidea-plugin link ${ROOT}"
}

case "$MODE" in
  local)   build_local ;;
  docker)
    build_docker
    package_plugin
    ;;
  check)   check_only ;;
  package) package_plugin ;;
  all)
    build_docker
    package_plugin
    check_only
    ;;
  *)
    echo "Usage: $0 [local|docker|check|package|all]" >&2
    exit 2
    ;;
esac
