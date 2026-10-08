#!/usr/bin/env bash
# Sourced by install.sh. All snapshots are taken BEFORE executable replacement.
# Helpers use the installer's as_root wrapper and transaction variables.

installer_systemd_available() {
  [ "${SYSTEMD_DIR}" = "/etc/systemd/system" ] &&
    [ -d /run/systemd/system ] &&
    command -v systemctl >/dev/null 2>&1
}

snapshot_install_transaction() {
  TRANSACTION_DIR="${TMP_DIR}/transaction"
  as_root install -d -m 0700 "${TRANSACTION_DIR}"

  local role unit existed
  for role in tx rx; do
    unit="${SYSTEMD_DIR}/avoverip-${role}.service"
    existed=false
    if [ -e "${unit}" ] || [ -L "${unit}" ]; then
      as_root cp -Pp "${unit}" "${TRANSACTION_DIR}/unit-${role}"
      existed=true
    fi
    printf -v "${role^^}_UNIT_EXISTED" '%s' "${existed}"
  done

  if [ -e "${SHARE_DIR}" ] || [ -L "${SHARE_DIR}" ]; then
    as_root cp -a "${SHARE_DIR}" "${TRANSACTION_DIR}/share"
    SHARE_EXISTED=true
  fi

  if installer_systemd_available; then
    MANAGED_SERVICES_SNAPSHOT=true
    local enabled
    for role in tx rx; do
      if systemctl is-active --quiet "avoverip-${role}" 2>/dev/null; then
        printf -v "${role^^}_WAS_ACTIVE" '%s' true
      fi
      enabled="$(systemctl is-enabled "avoverip-${role}" 2>/dev/null || true)"
      case "${enabled}" in
        enabled|enabled-runtime)
          printf -v "${role^^}_WAS_ENABLED" '%s' true ;;
        disabled|masked|masked-runtime|static|indirect|not-found|generated|transient|"")
          printf -v "${role^^}_WAS_ENABLED" '%s' false ;;
        *)
          printf '[install] warning: unexpected prior service enable state for %s: %s\n' "${role}" "${enabled}" >&2 ;;
      esac
    done
  fi
}

restore_deployed_assets() {
  local result=0 role unit existed
  # Copy, rather than move, snapshots: keep originals for diagnosis on failure.
  if [ "${SHARE_EXISTED}" = true ]; then
    if ! as_root rm -rf "${SHARE_DIR}" ||
      ! as_root cp -a "${TRANSACTION_DIR}/share" "${SHARE_DIR}"; then
      printf '[install] ERROR: failed to restore previous shared assets\n' >&2
      result=1
    fi
  elif ! as_root rm -rf "${SHARE_DIR}"; then
    printf '[install] ERROR: failed to remove new shared assets\n' >&2
    result=1
  fi

  for role in tx rx; do
    unit="${SYSTEMD_DIR}/avoverip-${role}.service"
    case "${role}" in
      tx) existed="${TX_UNIT_EXISTED}" ;;
      rx) existed="${RX_UNIT_EXISTED}" ;;
    esac
    if [ "${existed}" = true ]; then
      if ! as_root rm -f "${unit}" ||
        ! as_root cp -Pp "${TRANSACTION_DIR}/unit-${role}" "${unit}"; then
        printf '[install] ERROR: failed to restore unit for %s\n' "${role}" >&2
        result=1
      fi
    elif ! as_root rm -f "${unit}"; then
      printf '[install] ERROR: failed to remove new unit for %s\n' "${role}" >&2
      result=1
    fi
  done
  return "${result}"
}

restore_service_states() {
  [ "${MANAGED_SERVICES_SNAPSHOT}" = true ] || return 0

  local result=0 role service was_active was_enabled
  if ! as_root systemctl daemon-reload; then
    printf '[install] ERROR: systemd daemon-reload failed during rollback\n' >&2
    result=1
  fi

  # First undo starts/enables that were not present before the upgrade.
  for role in tx rx; do
    service="avoverip-${role}"
    case "${role}" in
      tx) was_active="${TX_WAS_ACTIVE}"; was_enabled="${TX_WAS_ENABLED}" ;;
      rx) was_active="${RX_WAS_ACTIVE}"; was_enabled="${RX_WAS_ENABLED}" ;;
    esac
    if [ "${was_active}" = false ] &&
      systemctl is-active --quiet "${service}" 2>/dev/null; then
      if ! as_root systemctl stop "${service}"; then
        printf '[install] ERROR: cannot stop newly started %s during rollback\n' "${service}" >&2
        result=1
      fi
    fi
    if [ "${was_enabled}" = true ]; then
      if ! as_root systemctl enable "${service}"; then
        printf '[install] ERROR: cannot re-enable %s during rollback\n' "${service}" >&2
        result=1
      fi
    elif ! as_root systemctl disable "${service}"; then
      printf '[install] ERROR: cannot restore disabled state of %s\n' "${service}" >&2
      result=1
    fi
  done

  # Restored executable + restored unit must be running, even if the new one
  # had already been started successfully before a later restart failure.
  for role in tx rx; do
    service="avoverip-${role}"
    case "${role}" in
      tx) was_active="${TX_WAS_ACTIVE}" ;;
      rx) was_active="${RX_WAS_ACTIVE}" ;;
    esac
    if [ "${was_active}" = true ]; then
      if ! as_root systemctl restart "${service}" ||
        ! systemctl is-active --quiet "${service}" 2>/dev/null; then
        printf '[install] ERROR: failed to restart previous %s during rollback\n' "${service}" >&2
        result=1
      fi
    fi
  done
  return "${result}"
}
