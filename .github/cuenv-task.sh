#!/usr/bin/env bash
# Run one cuenv task in CI. cuenv evaluates env.cue before every task and
# gives up after a fixed 10 seconds (MODULE_EVAL_TIMEOUT in cuenv's
# cuengine), which a cold evaluation on a hosted runner sometimes exceeds.
# Retry only that failure; a task that ran and failed is never retried.
set -uo pipefail

log=$(mktemp)
trap 'rm -f "$log"' EXIT
status=1
for attempt in 1 2 3; do
  cuenv task "$@" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  if [ "$status" -eq 0 ] || ! grep -q "CUE evaluation timed out" "$log"; then
    exit "$status"
  fi
  echo "cuenv evaluation timed out on attempt $attempt; retrying" >&2
done
exit "$status"
