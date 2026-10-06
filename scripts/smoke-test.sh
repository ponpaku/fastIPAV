#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

TX_BIN="${TX_BIN:-${REPO_ROOT}/target/debug/tx}"
RX_BIN="${RX_BIN:-${REPO_ROOT}/target/debug/rx}"
TMP_DIR="$(mktemp -d)"
TX_LOG="${TMP_DIR}/tx.log"
RX_LOG="${TMP_DIR}/rx.log"
tx_pid=""
rx_pid=""

cleanup() {
  local status=$?
  trap - EXIT

  for pid in "${tx_pid}" "${rx_pid}"; do
    if [ -n "${pid}" ] && kill -0 "${pid}" 2>/dev/null; then
      kill -TERM "${pid}" 2>/dev/null || true
    fi
  done
  for pid in "${tx_pid}" "${rx_pid}"; do
    if [ -n "${pid}" ]; then
      wait "${pid}" 2>/dev/null || true
    fi
  done

  if [ "${status}" -ne 0 ]; then
    printf '%s\n' '--- tx log ---' >&2
    cat "${TX_LOG}" >&2 2>/dev/null || true
    printf '%s\n' '--- rx log ---' >&2
    cat "${RX_LOG}" >&2 2>/dev/null || true
  fi

  rm -rf "${TMP_DIR}"
  exit "${status}"
}
trap cleanup EXIT

wait_for_health() {
  local role="$1"
  local url="$2"
  local pid="$3"
  local response=""

  for _ in $(seq 1 50); do
    if ! kill -0 "${pid}" 2>/dev/null; then
      printf '[smoke-test] %s exited before becoming healthy\n' "${role}" >&2
      return 1
    fi
    response="$(curl -fsS "${url}" 2>/dev/null || true)"
    if printf '%s' "${response}" | grep -q '"ok":true'; then
      printf '[smoke-test] %s healthy\n' "${role}"
      return 0
    fi
    sleep 0.2
  done

  printf '[smoke-test] timed out waiting for %s health endpoint\n' "${role}" >&2
  return 1
}

command -v cargo >/dev/null 2>&1 || {
  printf '[smoke-test] cargo is required\n' >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  printf '[smoke-test] curl is required\n' >&2
  exit 1
}

if [ ! -x "${TX_BIN}" ] || [ ! -x "${RX_BIN}" ]; then
  printf '[smoke-test] building debug binaries\n'
  cargo build --workspace --locked
fi

"${RX_BIN}" --config configs/rx.smoketest.toml >"${RX_LOG}" 2>&1 &
rx_pid=$!

"${TX_BIN}" --config configs/tx.smoketest.toml >"${TX_LOG}" 2>&1 &
tx_pid=$!

wait_for_health "tx" "http://127.0.0.1:18081/healthz" "${tx_pid}"
wait_for_health "rx" "http://127.0.0.1:18082/healthz" "${rx_pid}"

TX_STATS="$(curl -fsS "http://127.0.0.1:18081/stats")"
RX_STATS="$(curl -fsS "http://127.0.0.1:18082/stats")"

printf '%s' "${TX_STATS}" | grep -Eq '"frames_total":[1-9][0-9]*' || {
  printf '[smoke-test] tx reported no video buffers\n' >&2
  exit 1
}
printf '%s' "${RX_STATS}" | grep -Eq '"frames_total":[1-9][0-9]*' || {
  printf '[smoke-test] rx reported no decoded video buffers\n' >&2
  exit 1
}
printf '%s' "${TX_STATS}" | grep -Eq '"audio_chunks_total":[1-9][0-9]*' || {
  printf '[smoke-test] tx reported no audio buffers\n' >&2
  exit 1
}
printf '%s' "${RX_STATS}" | grep -Eq '"audio_chunks_total":[1-9][0-9]*' || {
  printf '[smoke-test] rx reported no decoded audio buffers\n' >&2
  exit 1
}

kill -TERM "${tx_pid}" "${rx_pid}"
wait "${tx_pid}"
wait "${rx_pid}"
tx_pid=""
rx_pid=""

printf '[smoke-test] tx/rx loopback smoke test passed with actual video and audio buffers\n'
