#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

REPO_SLUG="ponpaku/fastIPAV"
VERSION=""
INSTALL_DEPS=false
ENABLE_SERVICE=""
PREFIX="/usr/local"
CONFIG_DIR="/etc/avoverip"
SYSTEMD_DIR="/etc/systemd/system"
SHARE_DIR="${PREFIX}/share/fastipav"

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [options]

Options:
  --version <tag>           Install a specific release tag such as v0.1.0
  --install-deps            Install runtime dependencies with apt-get
  --enable-service <role>   Enable and start systemd service for tx, rx, or both
  --repo <owner/name>       Override GitHub repository slug
  --prefix <path>           Installation prefix for binaries and shared assets
  --config-dir <path>       Configuration directory
  --systemd-dir <path>      systemd unit directory
  -h, --help                Show this help

Examples:
  bash scripts/install.sh --install-deps
  bash scripts/install.sh --version v0.1.0 --enable-service rx
EOF
}

log() {
  printf '[install] %s\n' "$*"
}

fail() {
  printf '[install] error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

as_root() {
  if [ "${EUID}" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    fail "root privileges are required to run: $*"
  fi
}

normalize_arch() {
  case "$(uname -m)" in
    x86_64) printf 'x86_64' ;;
    aarch64|arm64) printf 'aarch64' ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
  esac
}

expected_target_for_arch() {
  case "$1" in
    x86_64) printf 'x86_64-unknown-linux-gnu' ;;
    aarch64) printf 'aarch64-unknown-linux-gnu' ;;
    *) fail "unsupported architecture: $1" ;;
  esac
}

detect_profile_suffix() {
  if [ -r /proc/device-tree/model ] &&
    tr -d '\0' </proc/device-tree/model | grep -qi 'raspberry pi'; then
    printf 'pi'
  else
    printf 'default'
  fi
}

resolve_latest_version() {
  local api_url="https://api.github.com/repos/${REPO_SLUG}/releases/latest"
  local response
  response="$(curl -fsSL "${api_url}")" ||
    fail "failed to query latest release from ${api_url}"
  printf '%s\n' "${response}" |
    sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' |
    head -n1
}

artifact_name() {
  local version="$1"
  local arch="$2"
  printf 'fastipav-%s-linux-%s.tar.gz' "${version}" "${arch}"
}

checksum_name() {
  local version="$1"
  local arch="$2"
  printf 'fastipav-%s-linux-%s.sha256' "${version}" "${arch}"
}

download_release_asset() {
  local asset_name="$1"
  local destination="$2"
  local asset_url="https://github.com/${REPO_SLUG}/releases/download/${VERSION}/${asset_name}"

  log "downloading ${asset_url}"
  if curl -fL "${asset_url}" -o "${destination}"; then
    return
  fi

  rm -f "${destination}"
  if command -v gh >/dev/null 2>&1; then
    log "curl download failed, trying gh release download"
    gh release download "${VERSION}" -R "${REPO_SLUG}"       -D "$(dirname "${destination}")" -p "${asset_name}" ||
      fail "failed to download release asset ${asset_name} with curl and gh"
  else
    fail "failed to download release asset ${asset_name}"
  fi
}

verify_package() {
  local package_path="$1"
  local checksum_path="$2"
  local expected
  local actual

  expected="$(awk 'NR == 1 { print $1 }' "${checksum_path}")"
  printf '%s\n' "${expected}" | grep -Eq '^[0-9a-fA-F]{64}$' ||
    fail "invalid checksum file: ${checksum_path}"
  actual="$(sha256sum "${package_path}" | awk '{ print $1 }')"
  [ "${actual}" = "${expected}" ] ||
    fail "checksum verification failed for ${package_path}"
  log "checksum verified for $(basename "${package_path}")"
}

install_deps() {
  log "installing runtime dependencies"
  as_root apt-get update
  as_root apt-get install -y \
    curl \
    ca-certificates \
    git \
    tar \
    gstreamer1.0-tools \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-libav \
    gstreamer1.0-alsa \
    gstreamer1.0-gl \
    gstreamer1.0-x \
    v4l-utils \
    alsa-utils
}

enable_and_restart_service() {
  local service="$1"
  as_root systemctl enable "${service}"
  as_root systemctl restart "${service}"
}

enable_services() {
  local role="$1"
  need_cmd systemctl
  as_root systemctl daemon-reload
  case "${role}" in
    tx)
      enable_and_restart_service avoverip-tx
      ;;
    rx)
      enable_and_restart_service avoverip-rx
      ;;
    both)
      enable_and_restart_service avoverip-tx
      enable_and_restart_service avoverip-rx
      ;;
    *)
      fail "invalid service role: ${role}"
      ;;
  esac
}

manifest_value() {
  local package_dir="$1"
  local key="$2"
  sed -n "s/^${key}=//p" "${package_dir}/manifest.txt" | head -n1
}

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[#&\\]/\\&/g'
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || fail "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --install-deps)
      INSTALL_DEPS=true
      shift
      ;;
    --enable-service)
      [ "$#" -ge 2 ] || fail "--enable-service requires a value"
      ENABLE_SERVICE="$2"
      shift 2
      ;;
    --repo)
      [ "$#" -ge 2 ] || fail "--repo requires a value"
      REPO_SLUG="$2"
      shift 2
      ;;
    --prefix)
      [ "$#" -ge 2 ] || fail "--prefix requires a value"
      PREFIX="$2"
      SHARE_DIR="${PREFIX}/share/fastipav"
      shift 2
      ;;
    --config-dir)
      [ "$#" -ge 2 ] || fail "--config-dir requires a value"
      CONFIG_DIR="$2"
      shift 2
      ;;
    --systemd-dir)
      [ "$#" -ge 2 ] || fail "--systemd-dir requires a value"
      SYSTEMD_DIR="$2"
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

[ "$(uname -s)" = "Linux" ] || fail "this installer supports Linux only"

for path_value in "${PREFIX}" "${CONFIG_DIR}" "${SYSTEMD_DIR}"; do
  case "${path_value}" in
    /*) ;;
    *) fail "install paths must be absolute: ${path_value}" ;;
  esac
  printf '%s\n' "${path_value}" | grep -Eq '^/[A-Za-z0-9._/-]+$' ||
    fail "install paths may only contain letters, numbers, '.', '_', '-', and '/': ${path_value}"
done

case "${ENABLE_SERVICE}" in
  ""|tx|rx|both) ;;
  *) fail "--enable-service must be tx, rx, or both" ;;
esac

if [ -n "${ENABLE_SERVICE}" ] && [ "${SYSTEMD_DIR}" != "/etc/systemd/system" ]; then
  fail "--enable-service requires --systemd-dir /etc/systemd/system"
fi

if [ "${INSTALL_DEPS}" = true ]; then
  need_cmd apt-get
  install_deps
fi

need_cmd curl
need_cmd tar
need_cmd install
need_cmd awk
need_cmd grep
need_cmd sed
need_cmd sha256sum

ARCH="$(normalize_arch)"
EXPECTED_TARGET="$(expected_target_for_arch "${ARCH}")"
PROFILE_SUFFIX="$(detect_profile_suffix)"

if [ "${PROFILE_SUFFIX}" = "default" ] &&
  { [ "${ENABLE_SERVICE}" = "rx" ] || [ "${ENABLE_SERVICE}" = "both" ]; }; then
  log "warning: Linux desktop RX usually needs a graphical-session environment; the system service is primarily suitable for KMS/headless-style sinks"
fi

if [ -z "${VERSION}" ]; then
  VERSION="$(resolve_latest_version)"
fi
[ -n "${VERSION}" ] || fail "failed to resolve release version"
printf '%s\n' "${VERSION}" |
  grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$' ||
  fail "invalid release version: ${VERSION}"

PACKAGE_NAME="$(artifact_name "${VERSION}" "${ARCH}")"
PACKAGE_BASENAME="${PACKAGE_NAME%.tar.gz}"
CHECKSUM_NAME="$(checksum_name "${VERSION}" "${ARCH}")"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT
LOCAL_PACKAGE="${REPO_ROOT}/dist/${PACKAGE_NAME}"
LOCAL_CHECKSUM="${REPO_ROOT}/dist/${CHECKSUM_NAME}"
PACKAGE_PATH="${TMP_DIR}/${PACKAGE_NAME}"
CHECKSUM_PATH="${TMP_DIR}/${CHECKSUM_NAME}"

if [ -f "${LOCAL_PACKAGE}" ]; then
  [ -f "${LOCAL_CHECKSUM}" ] ||
    fail "local checksum is missing: ${LOCAL_CHECKSUM}"
  log "using local package ${LOCAL_PACKAGE}"
  cp "${LOCAL_PACKAGE}" "${PACKAGE_PATH}"
  cp "${LOCAL_CHECKSUM}" "${CHECKSUM_PATH}"
else
  download_release_asset "${PACKAGE_NAME}" "${PACKAGE_PATH}"
  download_release_asset "${CHECKSUM_NAME}" "${CHECKSUM_PATH}"
fi

verify_package "${PACKAGE_PATH}" "${CHECKSUM_PATH}"

while IFS= read -r entry; do
  case "${entry}" in
    "${PACKAGE_BASENAME}"|"${PACKAGE_BASENAME}/"*) ;;
    *) fail "release archive contains unexpected path: ${entry}" ;;
  esac
  case "/${entry}/" in
    *"/../"*|*"/./"*) fail "release archive contains unsafe path: ${entry}" ;;
  esac
done < <(tar -tzf "${PACKAGE_PATH}")

tar -tvzf "${PACKAGE_PATH}" | awk '
  $1 !~ /^[d-]/ { bad = 1 }
  END { exit bad }
' || fail "release archive contains links or special file entries"

tar --no-same-owner --no-same-permissions -xzf "${PACKAGE_PATH}" -C "${TMP_DIR}"

PACKAGE_DIR="${TMP_DIR}/${PACKAGE_BASENAME}"
[ -d "${PACKAGE_DIR}" ] ||
  fail "release archive does not contain expected directory ${PACKAGE_BASENAME}"
[ -f "${PACKAGE_DIR}/manifest.txt" ] || fail "release manifest is missing"

MANIFEST_NAME="$(manifest_value "${PACKAGE_DIR}" name)"
MANIFEST_VERSION="$(manifest_value "${PACKAGE_DIR}" version)"
MANIFEST_TARGET="$(manifest_value "${PACKAGE_DIR}" target)"
MANIFEST_ARCH="$(manifest_value "${PACKAGE_DIR}" arch)"

[ "${MANIFEST_NAME}" = "${PACKAGE_BASENAME}" ] ||
  fail "release manifest name mismatch: expected ${PACKAGE_BASENAME}, got ${MANIFEST_NAME:-<empty>}"
[ "${MANIFEST_VERSION}" = "${VERSION}" ] ||
  fail "release manifest version mismatch: expected ${VERSION}, got ${MANIFEST_VERSION:-<empty>}"
[ "${MANIFEST_TARGET}" = "${EXPECTED_TARGET}" ] ||
  fail "release manifest target mismatch: expected ${EXPECTED_TARGET}, got ${MANIFEST_TARGET:-<empty>}"
[ "${MANIFEST_ARCH}" = "${ARCH}" ] ||
  fail "release manifest architecture mismatch: expected ${ARCH}, got ${MANIFEST_ARCH:-<empty>}"

for required in bin/tx bin/rx LICENSE; do
  [ -e "${PACKAGE_DIR}/${required}" ] ||
    fail "release package is missing ${required}"
done
[ -x "${PACKAGE_DIR}/bin/tx" ] || fail "packaged tx binary is not executable"
[ -x "${PACKAGE_DIR}/bin/rx" ] || fail "packaged rx binary is not executable"

TX_CONFIG="${PACKAGE_DIR}/configs/tx.${PROFILE_SUFFIX}.toml"
RX_CONFIG="${PACKAGE_DIR}/configs/rx.${PROFILE_SUFFIX}.toml"
[ -f "${TX_CONFIG}" ] || fail "release package is missing ${TX_CONFIG#"${PACKAGE_DIR}/"}"
[ -f "${RX_CONFIG}" ] || fail "release package is missing ${RX_CONFIG#"${PACKAGE_DIR}/"}"

TX_CONFIG_TO_CHECK="${TX_CONFIG}"
RX_CONFIG_TO_CHECK="${RX_CONFIG}"
if [ -f "${CONFIG_DIR}/tx.toml" ]; then
  TX_CONFIG_TO_CHECK="${CONFIG_DIR}/tx.toml"
fi
if [ -f "${CONFIG_DIR}/rx.toml" ]; then
  RX_CONFIG_TO_CHECK="${CONFIG_DIR}/rx.toml"
fi

log "validating effective config files with the new binaries"
as_root "${PACKAGE_DIR}/bin/tx" --config "${TX_CONFIG_TO_CHECK}" --check-config >/dev/null \
  || fail "tx config is not compatible with ${VERSION}: ${TX_CONFIG_TO_CHECK}"
as_root "${PACKAGE_DIR}/bin/rx" --config "${RX_CONFIG_TO_CHECK}" --check-config >/dev/null \
  || fail "rx config is not compatible with ${VERSION}: ${RX_CONFIG_TO_CHECK}"

log "installing binaries to ${PREFIX}/bin"
as_root install -d "${PREFIX}/bin"
as_root install -m 0755 "${PACKAGE_DIR}/bin/tx" "${PREFIX}/bin/tx"
as_root install -m 0755 "${PACKAGE_DIR}/bin/rx" "${PREFIX}/bin/rx"

log "installing shared assets to ${SHARE_DIR}"
as_root install -d "${SHARE_DIR}/configs" "${SHARE_DIR}/systemd"
as_root install -m 0644 "${PACKAGE_DIR}/LICENSE" "${SHARE_DIR}/LICENSE"
as_root cp -f "${PACKAGE_DIR}/configs/"*.toml "${SHARE_DIR}/configs/"
as_root cp -f "${PACKAGE_DIR}/systemd/"*.service "${SHARE_DIR}/systemd/"

log "installing default config files to ${CONFIG_DIR}"
as_root install -d "${CONFIG_DIR}"
if [ ! -f "${CONFIG_DIR}/tx.toml" ]; then
  as_root install -m 0644 "${TX_CONFIG}" "${CONFIG_DIR}/tx.toml"
else
  log "keeping existing ${CONFIG_DIR}/tx.toml"
fi
if [ ! -f "${CONFIG_DIR}/rx.toml" ]; then
  as_root install -m 0644 "${RX_CONFIG}" "${CONFIG_DIR}/rx.toml"
else
  log "keeping existing ${CONFIG_DIR}/rx.toml"
fi

PREFIX_SED="$(escape_sed_replacement "${PREFIX}")"
CONFIG_DIR_SED="$(escape_sed_replacement "${CONFIG_DIR}")"

log "installing systemd unit files to ${SYSTEMD_DIR}"
as_root install -d "${SYSTEMD_DIR}"
for role in tx rx; do
  unit_source="${PACKAGE_DIR}/systemd/avoverip-${role}.service"
  [ -f "${unit_source}" ] || fail "release package is missing systemd unit for ${role}"
  unit_rendered="${TMP_DIR}/avoverip-${role}.service"
  sed \
    -e "s#/usr/local/bin/#${PREFIX_SED}/bin/#g" \
    -e "s#/etc/avoverip/#${CONFIG_DIR_SED}/#g" \
    "${unit_source}" >"${unit_rendered}"
  as_root install -m 0644 "${unit_rendered}" "${SYSTEMD_DIR}/avoverip-${role}.service"
done

if [ "${SYSTEMD_DIR}" = "/etc/systemd/system" ] && command -v systemctl >/dev/null 2>&1; then
  as_root systemctl daemon-reload || true
fi

if [ -n "${ENABLE_SERVICE}" ]; then
  enable_services "${ENABLE_SERVICE}"
fi

USER_TO_CHECK="${SUDO_USER:-${USER:-}}"
if [ -n "${USER_TO_CHECK}" ] && command -v id >/dev/null 2>&1; then
  USER_GROUPS="$(id -nG "${USER_TO_CHECK}" 2>/dev/null || true)"
  if ! printf '%s\n' "${USER_GROUPS}" | grep -qw video; then
    log "note: ${USER_TO_CHECK} is not in the video group"
  fi
  if ! printf '%s\n' "${USER_GROUPS}" | grep -qw audio; then
    log "note: ${USER_TO_CHECK} is not in the audio group"
  fi
fi

log "installation complete"
log "binaries: ${PREFIX}/bin/tx and ${PREFIX}/bin/rx"
log "configs: ${CONFIG_DIR}/tx.toml and ${CONFIG_DIR}/rx.toml"
log "shared examples: ${SHARE_DIR}/configs"
if [ -z "${ENABLE_SERVICE}" ]; then
  log "services were not enabled automatically; use systemctl enable --now avoverip-{tx,rx} when ready"
fi
