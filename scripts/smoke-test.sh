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
retry_pid=""

cleanup() {
  local status=$?
  trap - EXIT

  for pid in "${tx_pid}" "${rx_pid}" "${retry_pid}"; do
    if [ -n "${pid}" ] && kill -0 "${pid}" 2>/dev/null; then
      kill -TERM "${pid}" 2>/dev/null || true
    fi
  done
  for pid in "${tx_pid}" "${rx_pid}" "${retry_pid}"; do
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

json_u64_field() {
  local json="$1"
  local field="$2"
  printf '%s' "${json}" |
    sed -n "s/.*\"${field}\":\([0-9][0-9]*\).*/\1/p"
}

wait_for_unhealthy() {
  local role="$1"
  local url="$2"
  local pid="$3"
  local status=""

  for _ in $(seq 1 60); do
    if ! kill -0 "${pid}" 2>/dev/null; then
      printf '[smoke-test] %s exited while waiting for unhealthy state\n' "${role}" >&2
      return 1
    fi
    status="$(curl -sS -o /dev/null -w '%{http_code}' "${url}" 2>/dev/null || true)"
    if [ "${status}" = "503" ]; then
      printf '[smoke-test] %s reported unhealthy after media loss\n' "${role}"
      return 0
    fi
    sleep 0.2
  done

  printf '[smoke-test] timed out waiting for %s to report media loss\n' "${role}" >&2
  return 1
}

command -v curl >/dev/null 2>&1 || {
  printf '[smoke-test] curl is required\n' >&2
  exit 1
}

if [ ! -x "${TX_BIN}" ] || [ ! -x "${RX_BIN}" ]; then
  command -v cargo >/dev/null 2>&1 || {
    printf '[smoke-test] cargo is required when test binaries are not already available\n' >&2
    exit 1
  }
  printf '[smoke-test] building debug binaries\n'
  cargo build --workspace --locked
fi

RETRY_CONFIG="${TMP_DIR}/tx.retry.toml"
sed \
  -e 's/interface = "lo"/interface = "fastipav-missing0"/' \
  -e 's/port = 18081/port = 18083/' \
  configs/tx.smoketest.toml >"${RETRY_CONFIG}"

"${TX_BIN}" --config "${RETRY_CONFIG}" >"${TMP_DIR}/tx-retry.log" 2>&1 &
retry_pid=$!
wait_for_unhealthy "tx-startup-retry" "http://127.0.0.1:18083/healthz" "${retry_pid}"
sleep 2
RETRY_STATS="$(curl -fsS "http://127.0.0.1:18083/stats")"
printf '%s' "${RETRY_STATS}" | grep -Eq '"pipeline_restarts":[1-9][0-9]*' || {
  printf '[smoke-test] startup retry did not increment pipeline_restarts\n' >&2
  exit 1
}
kill -TERM "${retry_pid}"
wait "${retry_pid}"
retry_pid=""

"${RX_BIN}" --config configs/rx.smoketest.toml >"${RX_LOG}" 2>&1 &
rx_pid=$!

# Receiver must be allowed to wait longer than media_timeout_ms for a sender.
sleep 6
if ! kill -0 "${rx_pid}" 2>/dev/null; then
  printf '[smoke-test] rx exited while transmitter was offline\n' >&2
  exit 1
fi
RX_WAIT_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:18082/healthz" 2>/dev/null || true)"
[ "${RX_WAIT_STATUS}" = "503" ] || {
  printf '[smoke-test] rx should remain unhealthy while waiting for transmitter\n' >&2
  exit 1
}
RX_WAIT_STATS="$(curl -fsS "http://127.0.0.1:18082/stats")"
printf '%s' "${RX_WAIT_STATS}" | grep -q '"pipeline_restarts":0' || {
  printf '[smoke-test] rx restarted while merely waiting for an offline transmitter\n' >&2
  exit 1
}

# Once one expected stream arrives, the receiver must not wait forever for
# the missing companion stream. Exercise video-only TX against audio-enabled RX.
VIDEO_ONLY_CONFIG="${TMP_DIR}/tx.video-only.toml"
sed 's/enabled = true/enabled = false/' configs/tx.smoketest.toml >"${VIDEO_ONLY_CONFIG}"
"${TX_BIN}" --config "${VIDEO_ONLY_CONFIG}" >"${TX_LOG}" 2>&1 &
tx_pid=$!
wait_for_health "tx-video-only" "http://127.0.0.1:18081/healthz" "${tx_pid}"
sleep 7
RX_PARTIAL_STATS="$(curl -fsS "http://127.0.0.1:18082/stats")"
printf '%s' "${RX_PARTIAL_STATS}" | grep -Eq '"pipeline_restarts":[1-9][0-9]*' || {
  printf '[smoke-test] rx did not recover from partial video-only media\n' >&2
  exit 1
}
kill -TERM "${tx_pid}"
wait "${tx_pid}"
tx_pid=""

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

# Verify that an established receiver detects media loss, restarts, and recovers.
RX_STATS="$(curl -fsS "http://127.0.0.1:18082/stats")"
RX_RESTARTS_BEFORE="$(json_u64_field "${RX_STATS}" pipeline_restarts)"
[ -n "${RX_RESTARTS_BEFORE}" ] || {
  printf '[smoke-test] could not read rx pipeline_restarts before media loss\n' >&2
  exit 1
}

kill -TERM "${tx_pid}"
wait "${tx_pid}"
tx_pid=""

wait_for_unhealthy "rx" "http://127.0.0.1:18082/healthz" "${rx_pid}"
for _ in $(seq 1 20); do
  RX_STATS="$(curl -fsS "http://127.0.0.1:18082/stats")"
  RX_RESTARTS_AFTER="$(json_u64_field "${RX_STATS}" pipeline_restarts)"
  if [ -n "${RX_RESTARTS_AFTER}" ] &&
    [ "${RX_RESTARTS_AFTER}" -gt "${RX_RESTARTS_BEFORE}" ]; then
    break
  fi
  sleep 0.2
done
[ -n "${RX_RESTARTS_AFTER:-}" ] &&
  [ "${RX_RESTARTS_AFTER}" -gt "${RX_RESTARTS_BEFORE}" ] || {
  printf '[smoke-test] rx restart count did not increase after established media loss\n' >&2
  exit 1
}

"${TX_BIN}" --config configs/tx.smoketest.toml >"${TX_LOG}" 2>&1 &
tx_pid=$!
wait_for_health "tx" "http://127.0.0.1:18081/healthz" "${tx_pid}"
wait_for_health "rx" "http://127.0.0.1:18082/healthz" "${rx_pid}"

kill -TERM "${tx_pid}" "${rx_pid}"
wait "${tx_pid}"
wait "${rx_pid}"
tx_pid=""
rx_pid=""

printf '[smoke-test] tx/rx media flow, stall detection, and recovery passed\n'
