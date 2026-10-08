#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

TX_BIN="${TX_BIN:-${REPO_ROOT}/target/debug/tx}"
RX_BIN="${RX_BIN:-${REPO_ROOT}/target/debug/rx}"
CONFIG_DIR="${CONFIG_DIR:-${REPO_ROOT}/configs}"
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

wait_for_restart_increment() {
  local role="$1"
  local url="$2"
  local pid="$3"
  local baseline="$4"
  local stats=""
  local current=""

  for _ in $(seq 1 60); do
    if ! kill -0 "${pid}" 2>/dev/null; then
      printf '[smoke-test] %s exited while waiting for pipeline restart\n' "${role}" >&2
      return 1
    fi
    stats="$(curl -fsS "${url}" 2>/dev/null || true)"
    current="$(json_u64_field "${stats}" pipeline_restarts)"
    if [ -n "${current}" ] && [ "${current}" -gt "${baseline}" ]; then
      printf '[smoke-test] %s pipeline restart observed\n' "${role}"
      return 0
    fi
    sleep 0.2
  done

  printf '[smoke-test] timed out waiting for %s pipeline restart\n' "${role}" >&2
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
  "${CONFIG_DIR}/tx.smoketest.toml" >"${RETRY_CONFIG}"

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

# Invalid GStreamer elements must not terminate the supervisor. The health
# endpoint stays available and startup construction failures are retried.
TX_BAD_PIPELINE_CONFIG="${TMP_DIR}/tx.bad-pipeline.toml"
sed \
  -e 's/port = 18081/port = 18087/' \
  -e 's#source_element = "videotestsrc name=video_src is-live=true pattern=smpte do-timestamp=true ! videoconvert ! jpegenc"#source_element = "fastipav_nonexistent_source"#' \
  "${CONFIG_DIR}/tx.smoketest.toml" >"${TX_BAD_PIPELINE_CONFIG}"

"${TX_BIN}" --config "${TX_BAD_PIPELINE_CONFIG}" >"${TMP_DIR}/tx-bad-pipeline.log" 2>&1 &
retry_pid=$!
wait_for_unhealthy "tx-construction-retry" "http://127.0.0.1:18087/healthz" "${retry_pid}"
wait_for_restart_increment \
  "tx-construction-retry" \
  "http://127.0.0.1:18087/stats" \
  "${retry_pid}" \
  0
kill -TERM "${retry_pid}"
wait "${retry_pid}"
retry_pid=""

RX_BAD_PIPELINE_CONFIG="${TMP_DIR}/rx.bad-pipeline.toml"
sed \
  -e 's/port = 18082/port = 18088/' \
  -e 's#sink_element = "fakesink sync=false async=false"#sink_element = "fastipav_nonexistent_sink"#' \
  "${CONFIG_DIR}/rx.smoketest.toml" >"${RX_BAD_PIPELINE_CONFIG}"

"${RX_BIN}" --config "${RX_BAD_PIPELINE_CONFIG}" >"${TMP_DIR}/rx-bad-pipeline.log" 2>&1 &
retry_pid=$!
wait_for_unhealthy "rx-construction-retry" "http://127.0.0.1:18088/healthz" "${retry_pid}"
wait_for_restart_increment \
  "rx-construction-retry" \
  "http://127.0.0.1:18088/stats" \
  "${retry_pid}" \
  0
kill -TERM "${retry_pid}"
wait "${retry_pid}"
retry_pid=""

ENCODER_DROP_CONFIG="${TMP_DIR}/tx.drop-encoder.toml"
sed \
  -e 's/port = 18081/port = 18085/' \
  -e 's/media_timeout_ms = 5000/media_timeout_ms = 1200/' \
  -e 's#encoder_element = "x264enc tune=zerolatency speed-preset=ultrafast"#encoder_element = "x264enc tune=zerolatency speed-preset=ultrafast ! identity drop-probability=1.0"#' \
  "${CONFIG_DIR}/tx.smoketest.toml" >"${ENCODER_DROP_CONFIG}"

"${TX_BIN}" --config "${ENCODER_DROP_CONFIG}" >"${TMP_DIR}/tx-drop-encoder.log" 2>&1 &
tx_pid=$!
wait_for_unhealthy "tx-encoder-drop" "http://127.0.0.1:18085/healthz" "${tx_pid}"
TX_DROP_STATS="$(curl -fsS "http://127.0.0.1:18085/stats")"
TX_DROP_RESTARTS_BEFORE="$(json_u64_field "${TX_DROP_STATS}" pipeline_restarts)"
[ -n "${TX_DROP_RESTARTS_BEFORE}" ] || {
  printf '[smoke-test] could not read tx encoder-drop restart count\n' >&2
  exit 1
}
wait_for_restart_increment \
  "tx-encoder-drop" \
  "http://127.0.0.1:18085/stats" \
  "${tx_pid}" \
  "${TX_DROP_RESTARTS_BEFORE}"
TX_DROP_STATS="$(curl -fsS "http://127.0.0.1:18085/stats")"
printf '%s' "${TX_DROP_STATS}" | grep -q 'encoder produced no H264' || {
  printf '[smoke-test] tx did not distinguish encoder output loss from source loss\n' >&2
  exit 1
}
kill -TERM "${tx_pid}"
wait "${tx_pid}"
tx_pid=""

"${RX_BIN}" --config "${CONFIG_DIR}/rx.smoketest.toml" >"${RX_LOG}" 2>&1 &
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

# One expected stream may arrive before the other. The receiver should stay
# unhealthy without rebuilding; restarting cannot create a missing companion stream
# and can prevent recovery while waiting for the next H.264 keyframe.
VIDEO_ONLY_CONFIG="${TMP_DIR}/tx.video-only.toml"
sed 's/enabled = true/enabled = false/' "${CONFIG_DIR}/tx.smoketest.toml" >"${VIDEO_ONLY_CONFIG}"
"${TX_BIN}" --config "${VIDEO_ONLY_CONFIG}" >"${TX_LOG}" 2>&1 &
tx_pid=$!
wait_for_health "tx-video-only" "http://127.0.0.1:18081/healthz" "${tx_pid}"
sleep 7
RX_PARTIAL_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:18082/healthz" 2>/dev/null || true)"
[ "${RX_PARTIAL_STATUS}" = "503" ] || {
  printf '[smoke-test] rx should stay unhealthy while waiting for the missing audio stream\n' >&2
  exit 1
}
RX_PARTIAL_STATS="$(curl -fsS "http://127.0.0.1:18082/stats")"
printf '%s' "${RX_PARTIAL_STATS}" | grep -q '"pipeline_restarts":0' || {
  printf '[smoke-test] rx restarted while waiting for the first audio stream\n' >&2
  exit 1
}
kill -TERM "${tx_pid}"
wait "${tx_pid}"
tx_pid=""

"${TX_BIN}" --config "${CONFIG_DIR}/tx.smoketest.toml" >"${TX_LOG}" 2>&1 &
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
if [ -z "${RX_RESTARTS_AFTER:-}" ] ||
  [ "${RX_RESTARTS_AFTER}" -le "${RX_RESTARTS_BEFORE}" ]; then
  printf '[smoke-test] rx restart count did not increase after established media loss\n' >&2
  exit 1
fi

"${TX_BIN}" --config "${CONFIG_DIR}/tx.smoketest.toml" >"${TX_LOG}" 2>&1 &
tx_pid=$!
wait_for_health "tx" "http://127.0.0.1:18081/healthz" "${tx_pid}"
wait_for_health "rx" "http://127.0.0.1:18082/healthz" "${rx_pid}"

kill -TERM "${tx_pid}" "${rx_pid}"
wait "${tx_pid}"
wait "${rx_pid}"
tx_pid=""
rx_pid=""

# RTP ingress without decoded video must not be mistaken for an offline TX.
# Drop decoded video after avdec_h264 while leaving RTP ingress intact.
DROP_RX_CONFIG="${TMP_DIR}/rx.drop-video.toml"
sed \
  -e 's/port = 18082/port = 18084/' \
  -e 's/media_timeout_ms = 5000/media_timeout_ms = 1200/' \
  -e 's#decoder_element = "avdec_h264"#decoder_element = "avdec_h264 ! identity drop-probability=1.0"#' \
  "${CONFIG_DIR}/rx.smoketest.toml" >"${DROP_RX_CONFIG}"

"${RX_BIN}" --config "${DROP_RX_CONFIG}" >"${TMP_DIR}/rx-drop-video.log" 2>&1 &
rx_pid=$!
sleep 0.5
DROP_RX_STATS="$(curl -fsS "http://127.0.0.1:18084/stats")"
DROP_RX_RESTARTS_BEFORE="$(json_u64_field "${DROP_RX_STATS}" pipeline_restarts)"
[ -n "${DROP_RX_RESTARTS_BEFORE}" ] || {
  printf '[smoke-test] could not read drop-video rx restart count\n' >&2
  exit 1
}

"${TX_BIN}" --config "${CONFIG_DIR}/tx.smoketest.toml" >"${TX_LOG}" 2>&1 &
tx_pid=$!
wait_for_health "tx-drop-video-test" "http://127.0.0.1:18081/healthz" "${tx_pid}"
wait_for_restart_increment   "rx-drop-video"   "http://127.0.0.1:18084/stats"   "${rx_pid}"   "${DROP_RX_RESTARTS_BEFORE}"

DROP_RX_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:18084/healthz" 2>/dev/null || true)"
[ "${DROP_RX_STATUS}" = "503" ] || {
  printf '[smoke-test] rx with decoded-video drop should remain unhealthy\n' >&2
  exit 1
}
DROP_RX_STATS="$(curl -fsS "http://127.0.0.1:18084/stats")"
printf '%s' "${DROP_RX_STATS}" | grep -q 'decoder produced no frames' || {
  printf '[smoke-test] rx did not report RTP ingress without decoded video\n' >&2
  exit 1
}

kill -TERM "${tx_pid}" "${rx_pid}"
wait "${tx_pid}"
wait "${rx_pid}"
tx_pid=""
rx_pid=""

printf '[smoke-test] tx/rx media flow, ingress detection, stall detection, and recovery passed\n'
