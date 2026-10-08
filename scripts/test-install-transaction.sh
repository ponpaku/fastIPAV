#!/usr/bin/env bash
set -euo pipefail

# Isolated transaction test: never interacts with the host systemd manager.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/install-transaction.sh
source "${SCRIPT_DIR}/install-transaction.sh"

ROOT="$(mktemp -d)"
trap 'rm -rf "${ROOT}"' EXIT
PREFIX="${ROOT}/usr/local"
SYSTEMD_DIR="${ROOT}/systemd"
SHARE_DIR="${PREFIX}/share/fastipav"
TMP_DIR="${ROOT}/tmp"
TRANSACTION_DIR=""
TX_UNIT_EXISTED=false
RX_UNIT_EXISTED=false
SHARE_EXISTED=false
TX_WAS_ACTIVE=false
RX_WAS_ACTIVE=false
TX_WAS_ENABLED=false
RX_WAS_ENABLED=false
MANAGED_SERVICES_SNAPSHOT=false
mkdir -p "${PREFIX}/bin" "${SYSTEMD_DIR}" "${SHARE_DIR}" "${TMP_DIR}" "${ROOT}/service"

as_root() { "$@"; }
installer_systemd_available() { return 0; }

systemctl() {
  local action="$1"
  shift
  case "${action}" in
    is-active)
      [ "$1" = "--quiet" ] || exit 20
      [ "$(cat "${ROOT}/service/$2.active")" = active ]
      ;;
    is-enabled)
      local state
      state="$(cat "${ROOT}/service/$1.enabled")"
      printf '%s\n' "${state}"
      [ "${state}" = enabled ]
      ;;
    enable|disable)
      if [ "${action}" = enable ]; then
        printf 'enabled\n' > "${ROOT}/service/$1.enabled"
      else
        printf 'disabled\n' > "${ROOT}/service/$1.enabled"
      fi
      ;;
    restart)
      if [ "$1" = avoverip-rx ] && [ -e "${ROOT}/fail-next-rx" ]; then
        rm -f "${ROOT}/fail-next-rx"
        printf 'inactive\n' > "${ROOT}/service/$1.active"
        return 1
      fi
      local role="${1#avoverip-}"
      printf 'active\n' > "${ROOT}/service/$1.active"
      cp "${PREFIX}/bin/${role}" "${ROOT}/service/$1.running"
      cp "${SYSTEMD_DIR}/$1.service" "${ROOT}/service/$1.unit"
      ;;
    stop)
      printf 'inactive\n' > "${ROOT}/service/$1.active"
      rm -f "${ROOT}/service/$1.running"
      ;;
    daemon-reload) printf 'reload\n' >> "${ROOT}/service/actions" ;;
    *) printf 'unknown systemctl operation: %s\n' "${action}" >&2; return 1 ;;
  esac
}

seed_old_install() {
  local active="$1" enabled="$2" role
  for role in tx rx; do
    printf 'old %s\n' "${role}" > "${PREFIX}/bin/${role}"
    printf 'old unit %s\n' "${role}" > "${SYSTEMD_DIR}/avoverip-${role}.service"
    printf '%s\n' "${active}" > "${ROOT}/service/avoverip-${role}.active"
    printf '%s\n' "${enabled}" > "${ROOT}/service/avoverip-${role}.enabled"
    if [ "${active}" = active ]; then
      cp "${PREFIX}/bin/${role}" "${ROOT}/service/avoverip-${role}.running"
    else
      rm -f "${ROOT}/service/avoverip-${role}.running"
    fi
  done
  printf 'old assets\n' > "${SHARE_DIR}/version.txt"
}

upgrade_then_fail() {
  local role
  cp "${PREFIX}/bin/tx" "${ROOT}/tx.backup"
  cp "${PREFIX}/bin/rx" "${ROOT}/rx.backup"
  snapshot_install_transaction
  for role in tx rx; do
    printf 'new %s\n' "${role}" > "${PREFIX}/bin/${role}"
    printf 'new unit %s\n' "${role}" > "${SYSTEMD_DIR}/avoverip-${role}.service"
  done
  printf 'new assets\n' > "${SHARE_DIR}/version.txt"
  systemctl enable avoverip-tx
  systemctl enable avoverip-rx
  systemctl restart avoverip-tx
  touch "${ROOT}/fail-next-rx"
  if systemctl restart avoverip-rx; then
    printf 'injected RX restart unexpectedly succeeded\n' >&2
    return 1
  fi

  # Same rollback order as install.sh's EXIT handler.
  mv -f "${ROOT}/tx.backup" "${PREFIX}/bin/tx"
  mv -f "${ROOT}/rx.backup" "${PREFIX}/bin/rx"
  restore_deployed_assets
  restore_service_states

  for role in tx rx; do
    [ "$(cat "${PREFIX}/bin/${role}")" = "old ${role}" ]
    [ "$(cat "${SYSTEMD_DIR}/avoverip-${role}.service")" = "old unit ${role}" ]
  done
  [ "$(cat "${SHARE_DIR}/version.txt")" = 'old assets' ]
  grep -qx reload "${ROOT}/service/actions"
}

# Critical scenario: TX restarted as new version, then RX restart failed.
seed_old_install active enabled
upgrade_then_fail
for role in tx rx; do
  [ "$(cat "${ROOT}/service/avoverip-${role}.active")" = active ]
  [ "$(cat "${ROOT}/service/avoverip-${role}.running")" = "old ${role}" ]
  [ "$(cat "${ROOT}/service/avoverip-${role}.unit")" = "old unit ${role}" ]
  [ "$(cat "${ROOT}/service/avoverip-${role}.enabled")" = enabled ]
done

# Also undo services that the failed upgrade newly enabled and started.
rm -rf "${TMP_DIR}/transaction"
TX_WAS_ACTIVE=false
RX_WAS_ACTIVE=false
TX_WAS_ENABLED=false
RX_WAS_ENABLED=false
TX_UNIT_EXISTED=false
RX_UNIT_EXISTED=false
SHARE_EXISTED=false
MANAGED_SERVICES_SNAPSHOT=false
seed_old_install inactive disabled
upgrade_then_fail
for role in tx rx; do
  [ "$(cat "${ROOT}/service/avoverip-${role}.active")" = inactive ]
  [ "$(cat "${ROOT}/service/avoverip-${role}.enabled")" = disabled ]
  [ ! -e "${ROOT}/service/avoverip-${role}.running" ]
done

printf '[test-install-transaction] rollback restores old binaries, units, assets, service state\n'
