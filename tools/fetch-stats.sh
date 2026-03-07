#!/usr/bin/env bash
set -eu

ADDR="${1:-127.0.0.1:8082}"
curl -fsS "http://${ADDR}/stats"
