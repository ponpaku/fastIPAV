#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh --version <tag> [--target <triple>] [--skip-build]

Examples:
  ./scripts/package-release.sh --version v0.1.0
  ./scripts/package-release.sh --version v0.1.0 --target aarch64-unknown-linux-gnu
EOF
}

log() {
  printf '[package-release] %s\n' "$*"
}

fail() {
  printf '[package-release] error: %s\n' "$*" >&2
  exit 1
}

VERSION=""
TARGET=""
SKIP_BUILD=false

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --target)
      TARGET="${2:-}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

[ -n "${VERSION}" ] || fail "--version is required"

if [ -z "${TARGET}" ]; then
  TARGET="$(rustc -vV | sed -n 's/^host: //p')"
fi

case "${TARGET}" in
  x86_64-unknown-linux-gnu)
    ARCH="x86_64"
    ;;
  aarch64-unknown-linux-gnu)
    ARCH="aarch64"
    ;;
  *)
    fail "unsupported target triple: ${TARGET}"
    ;;
esac

if [ "${SKIP_BUILD}" = false ]; then
  log "building release binaries for ${TARGET}"
  cargo build --release --locked --target "${TARGET}"
fi

BIN_DIR="target/${TARGET}/release"
[ -x "${BIN_DIR}/tx" ] || fail "missing binary: ${BIN_DIR}/tx"
[ -x "${BIN_DIR}/rx" ] || fail "missing binary: ${BIN_DIR}/rx"

PACKAGE_BASENAME="fastipav-${VERSION}-linux-${ARCH}"
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGE_DIR}"' EXIT
PACKAGE_DIR="${STAGE_DIR}/${PACKAGE_BASENAME}"

log "staging package in ${PACKAGE_DIR}"
install -d "${PACKAGE_DIR}/bin" "${PACKAGE_DIR}/configs" "${PACKAGE_DIR}/systemd"
install -m 0755 "${BIN_DIR}/tx" "${PACKAGE_DIR}/bin/tx"
install -m 0755 "${BIN_DIR}/rx" "${PACKAGE_DIR}/bin/rx"
cp configs/*.toml "${PACKAGE_DIR}/configs/"
cp systemd/*.service "${PACKAGE_DIR}/systemd/"

cat > "${PACKAGE_DIR}/manifest.txt" <<EOF
name=${PACKAGE_BASENAME}
version=${VERSION}
target=${TARGET}
arch=${ARCH}
EOF

mkdir -p dist
ARCHIVE_PATH="dist/${PACKAGE_BASENAME}.tar.gz"
CHECKSUM_PATH="dist/${PACKAGE_BASENAME}.sha256"

log "creating ${ARCHIVE_PATH}"
tar -C "${STAGE_DIR}" -czf "${ARCHIVE_PATH}" "${PACKAGE_BASENAME}"

if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${ARCHIVE_PATH}" > "${CHECKSUM_PATH}"
  log "wrote checksum ${CHECKSUM_PATH}"
fi

log "package created: ${ARCHIVE_PATH}"
