#!/bin/bash
set -euo pipefail
trap 'exit 0' TERM INT
case "${FIXTURE_MODE:-running}" in
  success) exit 0 ;;
  failure) exit 23 ;;
  running)
    while true; do sleep 1 & wait "$!" || true; done ;;
  *) exit 64 ;;
esac
