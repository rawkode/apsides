#!/usr/bin/env bash
# Shell-level fake-command probes for runner guards; no Docker or cluster needed.
set -euo pipefail
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bash -n "$ROOT/scripts/run-kind-e2e.sh"

TEMP=$(mktemp -d /tmp/oxideloop-kind-e2e-test.XXXXXX)
trap 'rm -rf "$TEMP"' EXIT
mkdir -p "$TEMP/bin" "$TEMP/repo/scripts" "$TEMP/repo/work/kind-e2e"
cp "$ROOT/scripts/run-kind-e2e.sh" "$TEMP/repo/scripts/"
cat >"$TEMP/bin/git" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
  'rev-parse HEAD') echo 0123456789012345678901234567890123456789 ;;
  'status --porcelain') printenv FAKE_GIT_STATUS 2>/dev/null || true ;;
  *) exit 0 ;;
esac
EOF
cat >"$TEMP/bin/kind" <<'EOF'
#!/usr/bin/env bash
if [ "$1 $2" = 'get clusters' ]; then
  if [ -n "${FAKE_DELETE_MARKER:-}" ] && [ -f "$FAKE_DELETE_MARKER" ]; then printenv FAKE_REMAINING_CLUSTER; else printenv FAKE_CLUSTERS 2>/dev/null || true; fi
fi
if [ "$1 $2" = 'create cluster' ]; then exit "${FAKE_KIND_CREATE_EXIT:-0}"; fi
if [ "$1 $2" = 'delete cluster' ]; then
  if [ "${FAKE_KIND_DELETE_REMAINS:-0}" = 1 ]; then : >"$FAKE_DELETE_MARKER"; fi
  exit "${FAKE_KIND_DELETE_EXIT:-0}"
fi
EOF
cat >"$TEMP/bin/docker" <<'EOF'
#!/usr/bin/env bash
if [ "$1" = inspect ] && [ "${3:-}" = --format ]; then
  echo 'kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48'
elif [ "$1" = inspect ]; then
  echo '{}'
fi
exit 0
EOF
for command in kubectl pnpm; do
  printf '#!/usr/bin/env bash\nexit 0\n' >"$TEMP/bin/$command"
done
chmod +x "$TEMP/bin/"*

run_guard() {
  expected=$1; shift
  set +e
  (cd "$TEMP/repo" && PATH="$TEMP/bin:$PATH" "$@") >"$TEMP/out" 2>&1
  result=$?
  set -e
  [ "$result" -ne 0 ] && grep -Fq "$expected" "$TEMP/out"
}

assert_contains() { grep -Fq "$1" "$ROOT/scripts/run-kind-e2e.sh" || { echo "missing runner contract: $1" >&2; exit 1; }; }
assert_not_contains() { ! grep -Fq "$1" "$ROOT/scripts/run-kind-e2e.sh" || { echo "obsolete or vacuous runner contract: $1" >&2; exit 1; }; }

mkdir -p "$TEMP/repo/work/kind-e2e/existing"
run_guard 'evidence path already exists' env RUN_ID=existing scripts/run-kind-e2e.sh
rmdir "$TEMP/repo/work/kind-e2e/existing"
run_guard 'cluster already exists' env RUN_ID=fresh FAKE_CLUSTERS=oxideloop-e2e-fresh scripts/run-kind-e2e.sh
run_guard 'refusing nonignored working-tree changes' env RUN_ID=fresh FAKE_GIT_STATUS='?? stray' scripts/run-kind-e2e.sh
run_guard 'RUN_ID must contain only lowercase' env RUN_ID=Uppercase scripts/run-kind-e2e.sh

# Stop immediately after the generated ID is logged. This is an executable
# regression check for the lowercase default, without needing a cluster.
set +e
(cd "$TEMP/repo" && PATH="$TEMP/bin:$PATH" FAKE_KIND_CREATE_EXIT=78 scripts/run-kind-e2e.sh) >"$TEMP/default-id.out" 2>&1
default_result=$?
set -e
[ "$default_result" -ne 0 ]
grep -Eq 'run=[0-9]{8}t[0-9]{6}z-[0-9]+' "$TEMP/default-id.out"

# Deliberately stop after fake cluster creation, then prove cleanup preserves
# a successful empty git-status capture instead of treating it as failure.
set +e
(cd "$TEMP/repo" && PATH="$TEMP/bin:$PATH" RUN_ID=empty-capture OXIDELOOP_E2E_SELF_TEST_STOP_AFTER_CLUSTER_CREATE=1 scripts/run-kind-e2e.sh) >"$TEMP/empty-capture.out" 2>&1
empty_capture_result=$?
set -e
[ "$empty_capture_result" -ne 0 ]
[ -f "$TEMP/repo/work/kind-e2e/empty-capture/git-status.txt" ]
[ ! -s "$TEMP/repo/work/kind-e2e/empty-capture/git-status.txt" ]
jq -e '.status == "failed" and .clusterDisposition == "deleted"' "$TEMP/repo/work/kind-e2e/empty-capture/result.json" >/dev/null

# Cluster cleanup is part of success, so an otherwise-successful self-test
# must become failed when the exact fresh cluster cannot be deleted.
set +e
(cd "$TEMP/repo" && PATH="$TEMP/bin:$PATH" RUN_ID=delete-failure FAKE_KIND_DELETE_EXIT=1 OXIDELOOP_E2E_SELF_TEST_FINISH_AFTER_CLUSTER_CREATE=1 scripts/run-kind-e2e.sh) >"$TEMP/delete-failure.out" 2>&1
delete_failure_result=$?
set -e
[ "$delete_failure_result" -ne 0 ]
jq -e '.status == "failed" and .failureReason == "kind cluster deletion failed" and .clusterDisposition == "delete-failed"' "$TEMP/repo/work/kind-e2e/delete-failure/result.json" >/dev/null

# A zero delete exit is insufficient if kind still lists the exact name.
set +e
(cd "$TEMP/repo" && PATH="$TEMP/bin:$PATH" RUN_ID=delete-remains FAKE_KIND_DELETE_REMAINS=1 FAKE_DELETE_MARKER="$TEMP/delete.marker" FAKE_REMAINING_CLUSTER=oxideloop-e2e-delete-remains OXIDELOOP_E2E_SELF_TEST_FINISH_AFTER_CLUSTER_CREATE=1 scripts/run-kind-e2e.sh) >"$TEMP/delete-remains.out" 2>&1
delete_remains_result=$?
set -e
[ "$delete_remains_result" -ne 0 ]
jq -e '.status == "failed" and .failureReason == "kind reported deletion but the exact cluster still exists" and .clusterDisposition == "delete-failed"' "$TEMP/repo/work/kind-e2e/delete-remains/result.json" >/dev/null

# These source-level assertions intentionally reject the regressions that a
# command fake cannot model: empty successful captures, list-index crash
# checks, vacuous image checks, and count-only automation/deletion evidence.
for token in \
  'date -u +%Y%m%dt%H%M%Sz' \
  'RUN_ID must contain only lowercase' \
  'exec 9<>"$FIFO"' \
  'wait_for_exact_crash' \
  'CRASH_POD_UID' \
  'normalize_image_id' \
  'controller and renderer image identity proof failed' \
  'assert_automation_transition' \
  'AutomationRuns before source creation' \
  'first present observation establishes the durable baseline' \
  'deletionCleanupReceipt' \
  'root-only deletion recovery' \
  'source revision changed during run' \
  'kind cluster deletion failed' \
  'artifact-inventory.json' \
  'kind-e2e-reproducibility/v1'; do
  assert_contains "$token"
done
assert_not_contains 'required capture was empty'
assert_not_contains 'map(select(.name == "controller") | .imageID | contains($expected)) | all'
assert_not_contains "'.items[0].status.containerStatuses[] | select(.name == \"controller\") | .lastState.terminated.exitCode == 75'"
assert_not_contains "wait_json 'dependency automation materialization' '.items | length == 1' automationrun"
