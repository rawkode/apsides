#!/usr/bin/env bash
set -euo pipefail
trap 'exit 0' TERM INT

mode="${REAKT_FIXTURE_MODE:-${1:-running}}"
case "$mode" in
  success) exit 0 ;;
  failure) exit 23 ;;
  readiness)
    sleep "${REAKT_READY_AFTER_SECONDS:-1}"
    : > /tmp/reakt-ready
    ;;
  running) ;;
  *) exit 64 ;;
esac

while true; do
  sleep 1 &
  wait "$!" || true
done
