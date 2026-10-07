#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh --version <tag> [--target <triple>]

Examples:
  bash scripts/package-release.sh --version v0.1.0
  bash scripts/package-release.sh --version v0.1.0 --target aarch64-unknown-linux-gnu
EOF
}

log() {
  printf '[package-release] %s\n' "$*"
}

fail() {
  printf '[package-release] error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

VERSION=""
TARGET=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || fail "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --target)
      [ "$#" -ge 2 ] || fail "--target requires a value"
      TARGET="$2"
      shift 2
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

[ "$(uname -s)" = "Linux" ] || fail "release packaging supports Linux only"

need_cmd cargo
need_cmd rustc
need_cmd grep
need_cmd cut
need_cmd sed
need_cmd git
need_cmd gzip
need_cmd install
need_cmd tar
need_cmd sha256sum

[ -f rust-toolchain.toml ] || fail "rust-toolchain.toml is missing"
PINNED_RUST="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' rust-toolchain.toml | head -n1)"
[ -n "${PINNED_RUST}" ] || fail "failed to read pinned Rust version"
ACTIVE_RUST="$(rustc --version | cut -d' ' -f2)"
[ "${ACTIVE_RUST}" = "${PINNED_RUST}" ] ||
  fail "rustc ${ACTIVE_RUST} does not match pinned release toolchain ${PINNED_RUST}"

[ -n "${VERSION}" ] || fail "--version is required"
printf '%s\n' "${VERSION}" |
  grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$' ||
  fail "--version must be a release tag such as v0.1.0"

WORKSPACE_VERSION="$(grep -m1 '^version = "' Cargo.toml | cut -d'"' -f2)"
[ -n "${WORKSPACE_VERSION}" ] ||
  fail "failed to read workspace version from Cargo.toml"
[ "${VERSION#v}" = "${WORKSPACE_VERSION}" ] ||
  fail "release tag ${VERSION} does not match workspace version ${WORKSPACE_VERSION}"

HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "${HOST_TARGET}" ] || fail "failed to determine Rust host target"
if [ -z "${TARGET}" ]; then
  TARGET="${HOST_TARGET}"
fi

if [ "${TARGET}" != "${HOST_TARGET}" ]; then
  fail "cross-compiling release packages is not supported by this script; run it on a native ${TARGET} host or use the GitHub Release workflow"
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

log "building release binaries for ${TARGET}"
cargo build --release --locked --target "${TARGET}"

BIN_DIR="target/${TARGET}/release"
[ -x "${BIN_DIR}/tx" ] || fail "missing binary: ${BIN_DIR}/tx"
[ -x "${BIN_DIR}/rx" ] || fail "missing binary: ${BIN_DIR}/rx"
[ -f LICENSE ] || fail "LICENSE is missing"

PACKAGE_BASENAME="fastipav-${VERSION}-linux-${ARCH}"
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGE_DIR}"' EXIT
PACKAGE_DIR="${STAGE_DIR}/${PACKAGE_BASENAME}"

log "staging package in ${PACKAGE_DIR}"
install -d "${PACKAGE_DIR}/bin" "${PACKAGE_DIR}/configs" "${PACKAGE_DIR}/systemd"
install -m 0755 "${BIN_DIR}/tx" "${PACKAGE_DIR}/bin/tx"
install -m 0755 "${BIN_DIR}/rx" "${PACKAGE_DIR}/bin/rx"
install -m 0644 LICENSE "${PACKAGE_DIR}/LICENSE"

shopt -s nullglob
config_files=(configs/*.toml)
unit_files=(systemd/*.service)
[ "${#config_files[@]}" -gt 0 ] || fail "no config files found"
[ "${#unit_files[@]}" -gt 0 ] || fail "no systemd units found"
cp "${config_files[@]}" "${PACKAGE_DIR}/configs/"
cp "${unit_files[@]}" "${PACKAGE_DIR}/systemd/"
shopt -u nullglob

cat >"${PACKAGE_DIR}/manifest.txt" <<EOF
name=${PACKAGE_BASENAME}
version=${VERSION}
target=${TARGET}
arch=${ARCH}
EOF

mkdir -p dist
ARCHIVE_PATH="dist/${PACKAGE_BASENAME}.tar.gz"
CHECKSUM_PATH="dist/${PACKAGE_BASENAME}.sha256"
rm -f "${ARCHIVE_PATH}" "${CHECKSUM_PATH}"

SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct HEAD)}"
printf '%s\n' "${SOURCE_DATE_EPOCH}" | grep -Eq '^[0-9]+
  sha256sum "${PACKAGE_BASENAME}.tar.gz" >"${PACKAGE_BASENAME}.sha256"
)
[ -s "${CHECKSUM_PATH}" ] || fail "failed to create checksum"
log "wrote checksum ${CHECKSUM_PATH}"
log "package created: ${ARCHIVE_PATH}"
 ||
  fail "SOURCE_DATE_EPOCH must be an integer Unix timestamp"

log "creating deterministic ${ARCHIVE_PATH}"
tar \
  --sort=name \
  --mtime="@${SOURCE_DATE_EPOCH}" \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  -C "${STAGE_DIR}" \
  -cf - "${PACKAGE_BASENAME}" |
  gzip -n >"${ARCHIVE_PATH}"

(
  cd dist
  sha256sum "${PACKAGE_BASENAME}.tar.gz" >"${PACKAGE_BASENAME}.sha256"
)
[ -s "${CHECKSUM_PATH}" ] || fail "failed to create checksum"
log "wrote checksum ${CHECKSUM_PATH}"
log "package created: ${ARCHIVE_PATH}"
