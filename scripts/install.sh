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
  ./scripts/install.sh --install-deps
  ./scripts/install.sh --version v0.1.0 --enable-service rx
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
    *)
      fail "unsupported architecture: $(uname -m)"
      ;;
  esac
}

detect_profile_suffix() {
  if [ -r /proc/device-tree/model ] && tr -d '\0' </proc/device-tree/model | grep -qi 'raspberry pi'; then
    printf 'pi'
  else
    printf 'default'
  fi
}

resolve_latest_version() {
  local api_url="https://api.github.com/repos/${REPO_SLUG}/releases/latest"
  local response
  response="$(curl -fsSL "${api_url}")" || fail "failed to query latest release from ${api_url}"
  printf '%s\n' "${response}" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1
}

artifact_name() {
  local version="$1"
  local arch="$2"
  printf 'fastipav-%s-linux-%s.tar.gz' "${version}" "${arch}"
}

install_deps() {
  log "installing runtime dependencies"
  as_root apt-get update
  as_root apt-get install -y \
    curl \
    ca-certificates \
    git \
    tar \
    libasound2 \
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

enable_services() {
  local role="$1"
  command -v systemctl >/dev/null 2>&1 || fail "systemctl not found"
  as_root systemctl daemon-reload
  case "${role}" in
    tx)
      as_root systemctl enable --now avoverip-tx
      ;;
    rx)
      as_root systemctl enable --now avoverip-rx
      ;;
    both)
      as_root systemctl enable --now avoverip-tx
      as_root systemctl enable --now avoverip-rx
      ;;
    *)
      fail "invalid service role: ${role}"
      ;;
  esac
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --install-deps)
      INSTALL_DEPS=true
      shift
      ;;
    --enable-service)
      ENABLE_SERVICE="${2:-}"
      shift 2
      ;;
    --repo)
      REPO_SLUG="${2:-}"
      shift 2
      ;;
    --prefix)
      PREFIX="${2:-}"
      SHARE_DIR="${PREFIX}/share/fastipav"
      shift 2
      ;;
    --config-dir)
      CONFIG_DIR="${2:-}"
      shift 2
      ;;
    --systemd-dir)
      SYSTEMD_DIR="${2:-}"
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

need_cmd curl
need_cmd tar
need_cmd install

ARCH="$(normalize_arch)"
PROFILE_SUFFIX="$(detect_profile_suffix)"

if [ -z "${VERSION}" ]; then
  VERSION="$(resolve_latest_version)"
fi

[ -n "${VERSION}" ] || fail "failed to resolve release version"

if [ "${INSTALL_DEPS}" = true ]; then
  install_deps
fi

PACKAGE_NAME="$(artifact_name "${VERSION}" "${ARCH}")"
DOWNLOAD_URL="https://github.com/${REPO_SLUG}/releases/download/${VERSION}/${PACKAGE_NAME}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT
LOCAL_PACKAGE="${REPO_ROOT}/dist/${PACKAGE_NAME}"

if [ -f "${LOCAL_PACKAGE}" ]; then
  log "using local package ${LOCAL_PACKAGE}"
  cp "${LOCAL_PACKAGE}" "${TMP_DIR}/${PACKAGE_NAME}"
else
  log "downloading ${DOWNLOAD_URL}"
  if ! curl -fL "${DOWNLOAD_URL}" -o "${TMP_DIR}/${PACKAGE_NAME}"; then
    if command -v gh >/dev/null 2>&1; then
      log "curl download failed, trying gh release download"
      gh release download "${VERSION}" -R "${REPO_SLUG}" -D "${TMP_DIR}" -p "${PACKAGE_NAME}" \
        || fail "failed to download release artifact with curl and gh"
    else
      fail "failed to download release artifact"
    fi
  fi
fi
tar -xzf "${TMP_DIR}/${PACKAGE_NAME}" -C "${TMP_DIR}"

PACKAGE_DIR="$(find "${TMP_DIR}" -mindepth 1 -maxdepth 1 -type d | head -n1)"
[ -n "${PACKAGE_DIR}" ] || fail "failed to locate extracted package directory"

log "installing binaries to ${PREFIX}/bin"
as_root install -d "${PREFIX}/bin"
as_root install -m 0755 "${PACKAGE_DIR}/bin/tx" "${PREFIX}/bin/tx"
as_root install -m 0755 "${PACKAGE_DIR}/bin/rx" "${PREFIX}/bin/rx"

log "installing shared assets to ${SHARE_DIR}"
as_root install -d "${SHARE_DIR}/configs" "${SHARE_DIR}/systemd"
as_root cp -f "${PACKAGE_DIR}/configs/"*.toml "${SHARE_DIR}/configs/"
as_root cp -f "${PACKAGE_DIR}/systemd/"*.service "${SHARE_DIR}/systemd/"

log "installing default config files to ${CONFIG_DIR}"
as_root install -d "${CONFIG_DIR}"
if [ ! -f "${CONFIG_DIR}/tx.toml" ]; then
  as_root install -m 0644 "${PACKAGE_DIR}/configs/tx.${PROFILE_SUFFIX}.toml" "${CONFIG_DIR}/tx.toml"
else
  log "keeping existing ${CONFIG_DIR}/tx.toml"
fi
if [ ! -f "${CONFIG_DIR}/rx.toml" ]; then
  as_root install -m 0644 "${PACKAGE_DIR}/configs/rx.${PROFILE_SUFFIX}.toml" "${CONFIG_DIR}/rx.toml"
else
  log "keeping existing ${CONFIG_DIR}/rx.toml"
fi

log "installing systemd unit files to ${SYSTEMD_DIR}"
as_root install -d "${SYSTEMD_DIR}"
as_root install -m 0644 "${PACKAGE_DIR}/systemd/avoverip-tx.service" "${SYSTEMD_DIR}/avoverip-tx.service"
as_root install -m 0644 "${PACKAGE_DIR}/systemd/avoverip-rx.service" "${SYSTEMD_DIR}/avoverip-rx.service"
as_root systemctl daemon-reload || true

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
