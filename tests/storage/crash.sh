#!/bin/sh
set -eu

case "${APS_STORAGE_TEST_NONCE:-}" in
  ''|*[!a-f0-9]*) exit 2 ;;
esac
test "${#APS_STORAGE_TEST_NONCE}" -eq 32
test "${APS_STORAGE_TEST_PHASE:-}" = crash-ready

output=$(mktemp)
child=
cleanup() {
  if test -n "$child"; then
    kill -KILL "$child" 2>/dev/null || :
    wait "$child" 2>/dev/null || :
  fi
  rm -f "$output"
}
trap cleanup EXIT
trap 'exit 2' INT TERM

/usr/local/bin/inventory-pvc-probe pvc_inventory_phase --exact --ignored --nocapture > "$output" 2>&1 &
child=$!
attempt=0
while ! grep '^APS_STORAGE_CRASH_READY=' "$output" | grep -Fq "\"nonce\":\"$APS_STORAGE_TEST_NONCE\""; do
  kill -0 "$child"
  attempt=$((attempt + 1))
  test "$attempt" -lt 60
  sleep 1
done
test "$(readlink "/proc/$child/exe")" = /usr/local/bin/inventory-pvc-probe
kill -KILL "$child"
set +e
wait "$child" 2>/dev/null
status=$?
child=
set -e
test "$status" -eq 137
grep '^APS_STORAGE_CRASH_READY=' "$output"
printf 'APS_STORAGE_KILLED={"nonce":"%s","signal":9,"waitStatus":137,"reaped":true}\n' "$APS_STORAGE_TEST_NONCE"
