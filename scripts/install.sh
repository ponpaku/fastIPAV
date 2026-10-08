#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

REPO_SLUG="ponpaku/fastIPAV"
VERSION=""
INSTALL_DEPS=false
USE_LOCAL_DIST=false
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
  --local-dist              Use matching package/checksum from ./dist instead of GitHub
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
  local version

  if response="$(curl -fsSL "${api_url}")"; then
    version="$(
      printf '%s\n' "${response}" |
        sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -n1
    )"
    if [ -n "${version}" ]; then
      printf '%s' "${version}"
      return
    fi
  fi

  if command -v gh >/dev/null 2>&1; then
    gh api "repos/${REPO_SLUG}/releases/latest" --jq '.tag_name' ||
      fail "failed to query latest release for ${REPO_SLUG} with curl and gh"
    return
  fi

  fail "failed to query latest release from ${api_url}"
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
    rm -f "${destination}"
    gh release download "${VERSION}" -R "${REPO_SLUG}" \
      -D "$(dirname "${destination}")" -p "${asset_name}" ||
      fail "failed to download release asset ${asset_name} with curl and gh"
  else
    fail "failed to download release asset ${asset_name}"
  fi
}

verify_package() {
  local package_path="$1"
  local checksum_path="$2"
  local expected
  local checksum_file
  local actual
  local nonempty_lines

  nonempty_lines="$(awk 'NF { count++ } END { print count + 0 }' "${checksum_path}")"
  [ "${nonempty_lines}" -eq 1 ] ||
    fail "checksum file must contain exactly one entry: ${checksum_path}"

  expected="$(awk 'NF { print $1 }' "${checksum_path}")"
  checksum_file="$(awk 'NF { print $2 }' "${checksum_path}")"
  checksum_file="${checksum_file#\*}"
  printf '%s\n' "${expected}" | grep -Eq '^[0-9a-fA-F]{64}$' ||
    fail "invalid checksum file: ${checksum_path}"
  [ "${checksum_file}" = "$(basename "${package_path}")" ] ||
    fail "checksum filename mismatch: expected $(basename "${package_path}"), got ${checksum_file:-<empty>}"

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

ensure_service_groups() {
  need_cmd getent
  local group
  for group in video audio; do
    getent group "${group}" >/dev/null 2>&1 ||
      fail "required system group is missing: ${group}"
  done
}

ensure_service_user() {
  need_cmd getent
  need_cmd id

  if ! getent group avoverip >/dev/null 2>&1; then
    need_cmd groupadd
    log "creating system group avoverip"
    as_root groupadd --system avoverip
  else
    local existing_gid
    existing_gid="$(getent group avoverip | awk -F: '{ print $3 }')"
    if [ -z "${existing_gid}" ] || [ "${existing_gid}" -eq 0 ] || [ "${existing_gid}" -ge 1000 ]; then
      fail "existing group avoverip (gid ${existing_gid:-unknown}) is not a dedicated system group"
    fi
  fi

  if id -u avoverip >/dev/null 2>&1; then
    local existing_uid
    existing_uid="$(id -u avoverip)"
    if [ "${existing_uid}" -eq 0 ] || [ "${existing_uid}" -ge 1000 ]; then
      fail "existing user avoverip (uid ${existing_uid}) is not a dedicated system account"
    fi
    return
  fi

  need_cmd useradd
  log "creating system user avoverip"
  as_root useradd \
    --system \
    --gid avoverip \
    --no-create-home \
    --home-dir /nonexistent \
    --shell /usr/sbin/nologin \
    avoverip
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

restart_active_services() {
  local service
  for service in avoverip-tx avoverip-rx; do
    if systemctl is-active --quiet "${service}" 2>/dev/null; then
      log "restarting active service ${service}"
      as_root systemctl restart "${service}"
    fi
  done
}

manifest_value() {
  local package_dir="$1"
  local key="$2"
  local value
  value="$(
    awk -v key="${key}" '
      index($0, key "=") == 1 {
        count++
        value = substr($0, length(key) + 2)
      }
      END {
        if (count == 1) {
          print value
        } else {
          exit 1
        }
      }
    ' "${package_dir}/manifest.txt"
  )" || fail "release manifest must contain exactly one ${key}= entry"
  printf '%s' "${value}"
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
    --local-dist)
      USE_LOCAL_DIST=true
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

trim_trailing_slashes() {
  local value="$1"
  while [ "${value}" != "/" ] && [ "${value%/}" != "${value}" ]; do
    value="${value%/}"
  done
  printf '%s' "${value}"
}

PREFIX="$(trim_trailing_slashes "${PREFIX}")"
CONFIG_DIR="$(trim_trailing_slashes "${CONFIG_DIR}")"
SYSTEMD_DIR="$(trim_trailing_slashes "${SYSTEMD_DIR}")"
SHARE_DIR="${PREFIX}/share/fastipav"

for path_value in "${PREFIX}" "${CONFIG_DIR}" "${SYSTEMD_DIR}"; do
  case "${path_value}" in
    /*) ;;
    *) fail "install paths must be absolute: ${path_value}" ;;
  esac
  [ "${path_value}" != "/" ] || fail "install paths must not be the filesystem root"
  case "/${path_value#/}/" in
    *"/../"*|*"/./"*) fail "install paths must not contain '.' or '..' components: ${path_value}" ;;
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

# Reject an impossible service activation before downloading or modifying files.
if [ -n "${ENABLE_SERVICE}" ] && [ ! -d /run/systemd/system ]; then
  fail "--enable-service requires a running systemd manager"
fi

if [ "${INSTALL_DEPS}" = true ]; then
  need_cmd apt-get
  install_deps
fi

if [ "${USE_LOCAL_DIST}" != true ]; then
  need_cmd curl
fi
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

if [ "${USE_LOCAL_DIST}" = true ] && [ -z "${VERSION}" ]; then
  fail "--local-dist requires --version"
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
TX_STAGED=""
RX_STAGED=""
TX_BACKUP=""
RX_BACKUP=""
TX_EXISTED=false
RX_EXISTED=false
ACTIVATION_STARTED=false

restore_binary() {
  local role="$1"
  local backup="$2"
  local existed="$3"
  if [ "${existed}" = true ]; then
    if ! as_root mv -f "${backup}" "${PREFIX}/bin/${role}"; then
      printf '[install] ERROR: rollback failed for %s; previous binary retained at %s\n' \
        "${role}" "${backup}" >&2
      return 1
    fi
  else
    if ! as_root rm -f "${PREFIX}/bin/${role}"; then
      printf '[install] ERROR: cannot remove newly installed %s during rollback\n' \
        "${role}" >&2
      return 1
    fi
  fi
}

cleanup() {
  local status=$?
  local tx_restored=true
  local rx_restored=true
  trap - EXIT
  if [ "${status}" -ne 0 ] && [ "${ACTIVATION_STARTED}" = true ]; then
    printf '[install] installation failed after binary activation; restoring prior binaries\n' >&2
    restore_binary tx "${TX_BACKUP}" "${TX_EXISTED}" || tx_restored=false
    restore_binary rx "${RX_BACKUP}" "${RX_EXISTED}" || rx_restored=false
  fi
  if [ -n "${TX_BACKUP}" ] && [ "${tx_restored}" = true ]; then
    as_root rm -f "${TX_BACKUP}" || true
  fi
  if [ -n "${RX_BACKUP}" ] && [ "${rx_restored}" = true ]; then
    as_root rm -f "${RX_BACKUP}" || true
  fi
  if [ -n "${TX_STAGED}" ]; then
    as_root rm -f "${TX_STAGED}" || true
  fi
  if [ -n "${RX_STAGED}" ]; then
    as_root rm -f "${RX_STAGED}" || true
  fi
  rm -rf "${TMP_DIR}"
  exit "${status}"
}
trap cleanup EXIT

LOCAL_PACKAGE="${REPO_ROOT}/dist/${PACKAGE_NAME}"
LOCAL_CHECKSUM="${REPO_ROOT}/dist/${CHECKSUM_NAME}"
PACKAGE_PATH="${TMP_DIR}/${PACKAGE_NAME}"
CHECKSUM_PATH="${TMP_DIR}/${CHECKSUM_NAME}"

if [ "${USE_LOCAL_DIST}" = true ]; then
  [ -f "${LOCAL_PACKAGE}" ] || fail "local package is missing: ${LOCAL_PACKAGE}"
  [ -f "${LOCAL_CHECKSUM}" ] ||
    fail "local checksum is missing: ${LOCAL_CHECKSUM}"
  log "using explicitly requested local package ${LOCAL_PACKAGE}"
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

for required in \
  bin/tx \
  bin/rx \
  LICENSE \
  systemd/avoverip-tx.service \
  systemd/avoverip-rx.service; do
  [ -e "${PACKAGE_DIR}/${required}" ] ||
    fail "release package is missing ${required}"
done
[ -x "${PACKAGE_DIR}/bin/tx" ] || fail "packaged tx binary is not executable"
[ -x "${PACKAGE_DIR}/bin/rx" ] || fail "packaged rx binary is not executable"

EXPECTED_BINARY_VERSION="${VERSION#v}"
[ "$("${PACKAGE_DIR}/bin/tx" --version)" = "tx ${EXPECTED_BINARY_VERSION}" ] ||
  fail "packaged tx binary version does not match ${VERSION}"
[ "$("${PACKAGE_DIR}/bin/rx" --version)" = "rx ${EXPECTED_BINARY_VERSION}" ] ||
  fail "packaged rx binary version does not match ${VERSION}"

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

if [ "${SYSTEMD_DIR}" = "/etc/systemd/system" ]; then
  ensure_service_groups
  ensure_service_user
fi

log "installing default config files to ${CONFIG_DIR}"
as_root install -d -m 0755 "${CONFIG_DIR}"
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

log "preparing binary directory ${PREFIX}/bin"
as_root install -d -m 0755 "${PREFIX}/bin"

if [ "${SYSTEMD_DIR}" = "/etc/systemd/system" ]; then
  # Keep service configs readable by the dedicated daemon without exposing
  # custom pipeline fragments or embedded URIs to every local user.
  as_root chown root:avoverip "${CONFIG_DIR}/tx.toml" "${CONFIG_DIR}/rx.toml"
  as_root chmod 0640 "${CONFIG_DIR}/tx.toml" "${CONFIG_DIR}/rx.toml"

  need_cmd runuser
  log "validating service-user access before replacing binaries"
  as_root runuser -u avoverip -- test -x "${PREFIX}/bin" ||
    fail "service user avoverip cannot traverse ${PREFIX}/bin"
  as_root runuser -u avoverip -- test -r "${CONFIG_DIR}/tx.toml" ||
    fail "service user avoverip cannot read ${CONFIG_DIR}/tx.toml"
  as_root runuser -u avoverip -- test -r "${CONFIG_DIR}/rx.toml" ||
    fail "service user avoverip cannot read ${CONFIG_DIR}/rx.toml"
fi

log "staging binaries in ${PREFIX}/bin"
TX_STAGED="${PREFIX}/bin/.tx.fastipav.new.${BASHPID}"
RX_STAGED="${PREFIX}/bin/.rx.fastipav.new.${BASHPID}"
as_root install -m 0755 "${PACKAGE_DIR}/bin/tx" "${TX_STAGED}"
as_root install -m 0755 "${PACKAGE_DIR}/bin/rx" "${RX_STAGED}"

if [ "${SYSTEMD_DIR}" = "/etc/systemd/system" ]; then
  log "validating staged binaries and configs as service user avoverip"
  as_root runuser -u avoverip -- "${TX_STAGED}" \
    --config "${CONFIG_DIR}/tx.toml" --check-config >/dev/null ||
    fail "staged tx binary/config is not usable by service user avoverip"
  as_root runuser -u avoverip -- "${RX_STAGED}" \
    --config "${CONFIG_DIR}/rx.toml" --check-config >/dev/null ||
    fail "staged rx binary/config is not usable by service user avoverip"
fi

# Preserve both existing binaries before replacing either. A failed second
# rename must not leave a mixed-version transmitter/receiver installation.
TX_BACKUP="${PREFIX}/bin/.tx.fastipav.previous.${BASHPID}"
RX_BACKUP="${PREFIX}/bin/.rx.fastipav.previous.${BASHPID}"
if [ -e "${PREFIX}/bin/tx" ] || [ -L "${PREFIX}/bin/tx" ]; then
  as_root cp -Pp "${PREFIX}/bin/tx" "${TX_BACKUP}"
  TX_EXISTED=true
fi
if [ -e "${PREFIX}/bin/rx" ] || [ -L "${PREFIX}/bin/rx" ]; then
  as_root cp -Pp "${PREFIX}/bin/rx" "${RX_BACKUP}"
  RX_EXISTED=true
fi

log "activating binaries in ${PREFIX}/bin"
ACTIVATION_STARTED=true
as_root mv -f "${TX_STAGED}" "${PREFIX}/bin/tx"
TX_STAGED=""
as_root mv -f "${RX_STAGED}" "${PREFIX}/bin/rx"
RX_STAGED=""

log "installing shared assets to ${SHARE_DIR}"
as_root install -d "${SHARE_DIR}/configs" "${SHARE_DIR}/systemd"
as_root install -m 0644 "${PACKAGE_DIR}/LICENSE" "${SHARE_DIR}/LICENSE"
as_root cp -f "${PACKAGE_DIR}/configs/"*.toml "${SHARE_DIR}/configs/"
as_root cp -f "${PACKAGE_DIR}/systemd/"*.service "${SHARE_DIR}/systemd/"

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
  if [ -d /run/systemd/system ]; then
    as_root systemctl daemon-reload
    if [ -z "${ENABLE_SERVICE}" ]; then
      restart_active_services
    fi
  else
    log "systemd manager is not running; unit files were installed but not reloaded"
  fi
fi

if [ -n "${ENABLE_SERVICE}" ]; then
  [ -d /run/systemd/system ] ||
    fail "--enable-service requires a running systemd manager"
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
