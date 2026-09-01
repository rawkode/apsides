#!/usr/bin/env bash
# Fresh-kind acceptance runner. It is intentionally bash 3 compatible.
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

NODE_IMAGE='kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48'
RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dt%H%M%Sz)-$$"}
CLUSTER_NAME=${KIND_CLUSTER_NAME:-"oxideloop-e2e-${RUN_ID}"}
NAMESPACE=${E2E_NAMESPACE:-oxideloop-e2e}
KEEP_CLUSTER=${KEEP_CLUSTER:-0}
TIMEOUT=${E2E_TIMEOUT_SECONDS:-180}
COLLISION_MODE=${E2E_COLLISION_MODE:-live}
EVIDENCE="$ROOT/work/kind-e2e/$RUN_ID"
WORK="$EVIDENCE/work"
RESULT="$EVIDENCE/result.json"
STATE_FILE="$EVIDENCE/state.json"
KUBECTL_TIMEOUT=${KUBECTL_TIMEOUT_SECONDS:-20}
CLUSTER_CREATED=false
BARRIER_PID=''
BARRIER_FD=''

die() { FAILURE="$*"; echo "run-kind-e2e: $FAILURE" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required command is unavailable: $1"; }
note() { printf '%s\n' "$*" | tee -a "$EVIDENCE/runner.log"; }

usage() {
  cat <<'EOF'
Usage: scripts/run-kind-e2e.sh

Creates a fresh disposable kind cluster, runs the serialized controller E2E
matrix, and writes inspectable evidence below work/kind-e2e/<RUN_ID>.

Environment: RUN_ID, KIND_CLUSTER_NAME, E2E_NAMESPACE, E2E_TIMEOUT_SECONDS,
E2E_COLLISION_MODE=live|restart, KEEP_CLUSTER=1, KIND_EXPERIMENTAL_PROVIDER,
and KUBECTL_TIMEOUT_SECONDS.
EOF
}
case "${1:-}" in -h|--help) usage; exit 0;; esac

need git; need kind; need kubectl; need docker; need jq; need pnpm
case "$RUN_ID" in *[!a-z0-9._-]*|'') die 'RUN_ID must contain only lowercase letters, digits, dot, underscore, or hyphen';; esac
case "$CLUSTER_NAME" in oxideloop-e2e-*);; *) die 'KIND_CLUSTER_NAME must begin oxideloop-e2e-';; esac
case "$CLUSTER_NAME" in *[!a-z0-9.-]*|*..*|.*|*.) die 'KIND_CLUSTER_NAME must be a DNS label';; esac
case "$NAMESPACE" in *[!a-z0-9-]*|*--*|-*|*-) die 'E2E_NAMESPACE must be a DNS label';; esac
[ "$TIMEOUT" -gt 0 ] 2>/dev/null || die 'E2E_TIMEOUT_SECONDS must be positive'
[ "$KUBECTL_TIMEOUT" -gt 0 ] 2>/dev/null || die 'KUBECTL_TIMEOUT_SECONDS must be positive'
case "$COLLISION_MODE" in
  live|restart) ;;
  *) die 'E2E_COLLISION_MODE must be live or restart' ;;
esac
if [ -e "$EVIDENCE" ]; then die "evidence path already exists: $EVIDENCE"; fi
if kind get clusters 2>/dev/null | awk -v name="$CLUSTER_NAME" '$0 == name { found=1 } END { exit !found }'; then die "cluster already exists: $CLUSTER_NAME"; fi
mkdir -p "$WORK"
hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}';
  else die "required command is unavailable: sha256sum or shasum"; fi
}

# A run must be attributable to an exact, entirely clean source tree. Ignored
# run evidence is deliberately outside porcelain status and is the sole write.
HEAD=$(git rev-parse HEAD)
DIRT=$(git status --porcelain --untracked-files=all)
[ -z "$DIRT" ] || { printf '%s\n' "$DIRT" >&2; die 'refusing nonignored working-tree changes'; }

# Keep provider selection local to this process. In particular, OrbStack's
# Docker-compatible daemon needs no global kind configuration.
if [ -z "${KIND_EXPERIMENTAL_PROVIDER:-}" ]; then
  docker info >/dev/null 2>&1 || die "Docker-compatible daemon is not ready (start Docker or OrbStack)"
  if command -v orbctl >/dev/null 2>&1 && orbctl status >/dev/null 2>&1; then
    export KIND_EXPERIMENTAL_PROVIDER=docker
    note "provider=docker (OrbStack detected)"
  else
    export KIND_EXPERIMENTAL_PROVIDER=docker
    note "provider=docker"
  fi
else
  note "provider=$KIND_EXPERIMENTAL_PROVIDER (caller supplied)"
fi

PASS=true
FAILURE=''
ARTIFACTS=''
OWNERSHIP_ARTIFACTS=''
atomic_capture() {
  name=$1; shift
  temporary="$EVIDENCE/.${name}.tmp"
  "$@" >"$temporary" 2>&1 || { rm -f "$temporary"; FAILURE="required capture failed: $name"; return 1; }
  mv "$temporary" "$EVIDENCE/$name"
  hash_file "$EVIDENCE/$name" >"$EVIDENCE/$name.sha256"
  ARTIFACTS="$ARTIFACTS $name"
}
atomic_capture_ownership() {
  name=$1; shift
  atomic_capture "$name" "$@" || return 1
  OWNERSHIP_ARTIFACTS="$OWNERSHIP_ARTIFACTS $name"
}
capture_collision_regression_boundary() {
  [ -f Cargo.toml ] || return 0
  need cargo
  atomic_capture collision-regression-tests.txt bash -c '
    set -euo pipefail
    printf "%s\n" "boundary: raw stale tombstone injection is not deterministic in kind; these exact core/runtime regressions are captured at the guarded revision"
    run_one() {
      package=$1; test_name=$2
      set +e
      output=$(cargo test --locked -p "$package" "$test_name" -- --exact --nocapture 2>&1)
      result=$?
      set -e
      printf "%s\n" "$output"
      [ "$result" -eq 0 ] || return 1
      test_count=$(printf "%s\n" "$output" | grep -c "^running 1 test$" || true)
      [ "$test_count" -eq 1 ] || {
        printf "%s\n" "targeted test did not execute exactly one test: $package $test_name" >&2
        return 1
      }
    }
    run_one oxideloop-controller-core reducer::tests::blocked_foreign_collision_root_deletion_preempts_through_normal_deletion_boundary
    run_one oxideloop-controller-runtime driver::tests::driver_root_deletion_preempts_blocked_collision_and_jsonl_is_correlated_and_deduped
    run_one oxideloop-controller-runtime driver::tests::driver_stale_blocked_collision_witness_is_illegal_quiescence
    run_one oxideloop-controller-runtime driver::tests::driver_ready_policy_dedupes_stable_jsonl_observation
    run_one oxideloop-controller-runtime driver::tests::raw_root_self_echo_does_not_invalidate_a_prewrite_session
    run_one oxideloop-controller-runtime driver::tests::real_transport_root_status_echo_restarts_the_full_prewrite_before_one_write
    run_one oxideloop-controller-runtime adapters::tests::stale_or_unpinned_delete_never_erases_a_replacement
  ' || die 'targeted collision regression tests failed'
}
ownership_reproducibility() {
  for name in $OWNERSHIP_ARTIFACTS; do
    jq -n --arg path "$name" --arg sha256 "$(cat "$EVIDENCE/$name.sha256")" '{path:$path,sha256:$sha256}'
  done | jq -s .
}
capture() {
  [ "$CLUSTER_CREATED" = true ] || return 0
  atomic_capture resources.yaml ctx get all,webapplications,oxideloopstates,automationruns,events.events.k8s.io -n "$NAMESPACE" -o yaml || return 1
  atomic_capture pods.txt ctx get pods -A -o wide || return 1
  atomic_capture pods.describe.txt ctx describe pods -n "$NAMESPACE" || return 1
  atomic_capture controller.log ctx logs deployment/oxideloop-controller -n "$NAMESPACE" --all-containers --tail=-1 || return 1
  atomic_capture events.txt ctx get events -A --sort-by=.lastTimestamp || return 1
  atomic_capture node.inspect.json docker inspect "${CLUSTER_NAME}-control-plane" || return 1
  atomic_capture source-revision.txt git rev-parse HEAD || return 1
  atomic_capture git-status.txt git status --porcelain --untracked-files=all || return 1
}
cleanup() {
  exit_code=$?
  if [ "$exit_code" -ne 0 ]; then PASS=false; FAILURE=${FAILURE:-"runner exited $exit_code"}; fi
  if [ -n "$BARRIER_PID" ]; then kill "$BARRIER_PID" >/dev/null 2>&1 || true; wait "$BARRIER_PID" 2>/dev/null || true; fi
  if [ -n "$BARRIER_FD" ]; then eval "exec ${BARRIER_FD}>&-"; fi
  capture || { PASS=false; FAILURE=${FAILURE:-'required evidence capture failed'}; }
  FINAL_HEAD=$(git rev-parse HEAD 2>/dev/null || true)
  [ "$FINAL_HEAD" = "$HEAD" ] || { PASS=false; FAILURE=${FAILURE:-'source revision changed during run'}; }
  FINAL_DIRT=$(git status --porcelain --untracked-files=all)
  [ -z "$FINAL_DIRT" ] || { PASS=false; FAILURE=${FAILURE:-'source tree became nonignored-dirty'}; }
  cluster_disposition=kept
  if [ "$CLUSTER_CREATED" = true ] && [ "$KEEP_CLUSTER" != 1 ]; then
    if kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1; then
      if clusters_after_delete=$(kind get clusters 2>/dev/null); then
        if printf '%s\n' "$clusters_after_delete" | awk -v name="$CLUSTER_NAME" '$0 == name { found=1 } END { exit !found }'; then
          PASS=false
          FAILURE=${FAILURE:-'kind reported deletion but the exact cluster still exists'}
          cluster_disposition=delete-failed
        else
          cluster_disposition=deleted
        fi
      else
        PASS=false
        FAILURE=${FAILURE:-'kind cluster deletion could not be verified'}
        cluster_disposition=delete-unverified
      fi
    else
      PASS=false
      FAILURE=${FAILURE:-'kind cluster deletion failed'}
      cluster_disposition=delete-failed
    fi
  fi
  status=passed; [ "$PASS" = true ] || status=failed
  image_ids=$(cat "$EVIDENCE/image-ids.json" 2>/dev/null || printf '{}')
  ownership_evidence=$(ownership_reproducibility)
  jq -n --arg runId "$RUN_ID" --arg cluster "$CLUSTER_NAME" --arg namespace "$NAMESPACE" --arg revision "$HEAD" --arg finalRevision "$FINAL_HEAD" --arg collisionMode "$COLLISION_MODE" --arg nodeImage "$NODE_IMAGE" --arg actualNodeImage "${ACTUAL_NODE_IMAGE:-}" --arg kindVersion "$(kind version 2>/dev/null || true)" --arg clusterDisposition "$cluster_disposition" --argjson images "$image_ids" --argjson ownershipEvidence "$ownership_evidence" --arg bundleDigest "${BUNDLE_DIGEST:-}" --arg bundleArchiveSha "${BUNDLE_SHA:-}" '{schema:"oxideloop.dev/kind-e2e-reproducibility/v1",runId:$runId,cluster:$cluster,namespace:$namespace,sourceRevision:$revision,finalSourceRevision:$finalRevision,mode:$collisionMode,collisionMode:$collisionMode,sourceTree:"clean-at-start-and-cleanup",nodeImage:{requested:$nodeImage,actual:$actualNodeImage},kindVersion:$kindVersion,clusterDisposition:$clusterDisposition,imageIds:$images,ownershipEvidence:$ownershipEvidence,bundle:{digest:$bundleDigest,archiveSha256:$bundleArchiveSha}}' >"$EVIDENCE/.reproducibility.json.tmp" && mv "$EVIDENCE/.reproducibility.json.tmp" "$EVIDENCE/reproducibility.json"
  (
    cd "$EVIDENCE"
    find . -type f ! -name '.artifact-inventory.json.tmp' ! -name 'artifact-inventory.json' ! -name 'artifact-inventory.json.sha256' ! -name 'result.json' ! -name 'result.json.sha256' | LC_ALL=C sort | while IFS= read -r file; do
      file=${file#./}
      jq -n --arg path "$file" --arg sha256 "$(hash_file "$EVIDENCE/$file")" --argjson bytes "$(wc -c <"$EVIDENCE/$file" | tr -d ' ')" '{path:$path,sha256:$sha256,bytes:$bytes}'
    done
  ) | jq -s '{schema:"oxideloop.dev/kind-e2e-artifact-inventory/v1",artifacts:.}' >"$EVIDENCE/.artifact-inventory.json.tmp" && mv "$EVIDENCE/.artifact-inventory.json.tmp" "$EVIDENCE/artifact-inventory.json"
  hash_file "$EVIDENCE/artifact-inventory.json" >"$EVIDENCE/artifact-inventory.json.sha256"
  inventory_sha=$(cat "$EVIDENCE/artifact-inventory.json.sha256")
  jq -n --arg runId "$RUN_ID" --arg cluster "$CLUSTER_NAME" --arg namespace "$NAMESPACE" --arg revision "$HEAD" --arg collisionMode "$COLLISION_MODE" --arg status "$status" --arg failure "$FAILURE" --arg evidence "$EVIDENCE" --arg nodeImage "$NODE_IMAGE" --arg kindVersion "$(kind version 2>/dev/null || true)" --arg clusterDisposition "$cluster_disposition" --argjson images "$image_ids" --arg artifacts "$ARTIFACTS" --arg inventory "artifact-inventory.json" --arg inventorySha256 "$inventory_sha" '{schema:"oxideloop.dev/kind-e2e-result/v1",runId:$runId,cluster:$cluster,namespace:$namespace,sourceRevision:$revision,mode:$collisionMode,collisionMode:$collisionMode,status:$status,failureReason:$failure,evidence:$evidence,nodeImage:$nodeImage,kindVersion:$kindVersion,clusterDisposition:$clusterDisposition,imageIds:$images,capturedArtifacts:($artifacts|split(" ")|map(select(length>0))),reproducibility:"reproducibility.json",artifactInventory:{path:$inventory,sha256:$inventorySha256}}' >"$RESULT.tmp" && mv "$RESULT.tmp" "$RESULT"
  hash_file "$RESULT" >"$RESULT.sha256"
  [ "$PASS" = true ] && [ "$exit_code" -eq 0 ] || exit 1
}
trap cleanup EXIT

ctx() { kubectl --request-timeout="${KUBECTL_TIMEOUT}s" --context "kind-$CLUSTER_NAME" "$@"; }
wait_json() {
  description=$1; filter=$2; resource=$3; shift 3; elapsed=0
  while ! ctx get "$resource" -n "$NAMESPACE" -o json 2>/dev/null | jq -e "$@" "$filter" >/dev/null; do
    [ "$elapsed" -lt "$TIMEOUT" ] || die "timed out waiting for $description"
    sleep 2; elapsed=$((elapsed + 2))
  done
}
assert_count() { actual=$1; expected=$2; label=$3; [ "$actual" = "$expected" ] || die "$label: expected $expected, got $actual"; }
wait_for_same_uid_changed_rv() {
  description=$1; resource=$2; expected_uid=$3; expected_rv=$4
  wait_json "$description" '(.metadata.uid == $expectedUid) and (.metadata.resourceVersion | type == "string" and . != $expectedRv)' "$resource" --arg expectedUid "$expected_uid" --arg expectedRv "$expected_rv"
}
managed_field_ownership_from_json() {
  json_file=$1; resource=$2; expected_subresource=$3; representative_field=$4
  jq -ec --arg resource "$resource" --arg expectedSubresource "$expected_subresource" --arg representativeField "$representative_field" '
    def exact_subresource:
      if $expectedSubresource == "main" then
        (if has("subresource") then .subresource == null else true end)
      else .subresource == $expectedSubresource end;
    def ownership_entry:
      (.manager == "oxideloop-controller") and
      (.operation == "Apply") and
      exact_subresource and
      (.fieldsType == "FieldsV1") and
      (.fieldsV1 | type == "object") and
      (.fieldsV1 | has($representativeField));
    if (.metadata | type) != "object" then error("managed-fields resource metadata is invalid") else . end |
    if (.metadata.managedFields | type) != "array" then error("managed-fields array is missing") else . end |
    [.metadata.managedFields[] | select(ownership_entry)] as $entries |
    if ($entries | length) == 0 then
      error("missing oxideloop-controller Apply ownership for " + $resource + " " + $expectedSubresource + " " + $representativeField)
    else $entries[0] as $entry |
      {schema:"oxideloop.dev/kind-e2e-managed-fields/v1",
       resource:$resource,
       uid:.metadata.uid,
       resourceVersion:.metadata.resourceVersion,
       manager:$entry.manager,
       operation:$entry.operation,
       subresource:($entry.subresource // null),
       fieldsType:$entry.fieldsType,
       representativeField:$representativeField}
    end
  ' "$json_file"
}
capture_managed_field_ownership() {
  label=$1; resource=$2; expected_subresource=$3; representative_field=$4
  raw="ownership-${label}-managed-fields.json"
  assertion="ownership-${label}-assertion.json"
  atomic_capture_ownership "$raw" ctx get "$resource" -n "$NAMESPACE" -o json --show-managed-fields || die "failed to capture $label managed-fields JSON"
  atomic_capture_ownership "$assertion" managed_field_ownership_from_json "$EVIDENCE/$raw" "$resource" "$expected_subresource" "$representative_field" || die "$label managed-fields ownership is not controller Apply/$expected_subresource/$representative_field"
}
ownership_restart_identities() {
  ctx get webapplication/root,oxideloopstate/"$STATE_NAME",configmap/root-config,service/root-service -n "$NAMESPACE" -o json | jq -Sec --arg stateName "$STATE_NAME" '
    def exact($kind; $name):
      [.items[] | select(.kind == $kind and .metadata.name == $name)] |
      if (length == 1 and (.[0].metadata.uid | type == "string")) then .[0] | {name:.metadata.name,uid:.metadata.uid} else error("missing exact " + $kind + "/" + $name + " identity") end;
    {root:exact("WebApplication"; "root"),state:exact("OxideLoopState"; $stateName),config:exact("ConfigMap"; "root-config"),service:exact("Service"; "root-service")}
  '
}
restart_controller() {
  label=$1
  RESTART_OLD_POD=$(controller_pod)
  RESTART_OLD_POD_UID=$(ctx get pod -n "$NAMESPACE" "$RESTART_OLD_POD" -o jsonpath='{.metadata.uid}')
  ctx delete pod -n "$NAMESPACE" "$RESTART_OLD_POD" --wait=false
  elapsed=0; RESTART_NEW_POD=''; RESTART_NEW_POD_UID=''
  while [ -z "$RESTART_NEW_POD" ]; do
    restart_snapshot=$(ctx get pods -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json 2>/dev/null || true)
    restart_pair=$(printf '%s' "$restart_snapshot" | jq -r --arg oldUid "$RESTART_OLD_POD_UID" '[.items[] | select(.metadata.deletionTimestamp == null and .metadata.uid != $oldUid and ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length == 1)) | [.metadata.name,.metadata.uid] | @tsv] | if length == 1 then .[0] else empty end' 2>/dev/null || true)
    if [ -n "$restart_pair" ]; then
      RESTART_NEW_POD=$(printf '%s' "$restart_pair" | cut -f1)
      RESTART_NEW_POD_UID=$(printf '%s' "$restart_pair" | cut -f2)
      break
    fi
    [ "$elapsed" -lt "$TIMEOUT" ] || die "timed out waiting for a new Ready controller pod after $label restart"
    sleep 2; elapsed=$((elapsed + 2))
  done
  [ "$RESTART_NEW_POD_UID" != "$RESTART_OLD_POD_UID" ] || die "$label restart reused the controller pod UID"
  ctx rollout status deployment/oxideloop-controller -n "$NAMESPACE" --timeout="${TIMEOUT}s"
}
controller_pod() {
  ctx get pod -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json |
    jq -er '[.items[] | select(.metadata.deletionTimestamp == null)] | if length == 1 then .[0].metadata.name else error("expected exactly one live controller pod") end'
}
root_uid() { ctx get webapplication root -n "$NAMESPACE" -o jsonpath='{.metadata.uid}'; }
controller_status() { ctx get pod -n "$NAMESPACE" "$1" -o json | jq -er '.status.containerStatuses[] | select(.name == "controller")'; }
wait_for_exact_crash() {
  point=$1; pod=$2; expected_uid=$3; expected_restart=$4; ticks=0; max_ticks=$((TIMEOUT * 10))
  while :; do
    snapshot=$(ctx get pod -n "$NAMESPACE" "$pod" -o json 2>/dev/null || true)
    actual_uid=$(printf '%s' "$snapshot" | jq -r '.metadata.uid // empty' 2>/dev/null || true)
    [ "$actual_uid" = "$expected_uid" ] || die "crash $point replaced the exact controller pod"
    status=$(printf '%s' "$snapshot" | jq -c '.status.containerStatuses[]? | select(.name == "controller")' 2>/dev/null || true)
    restart=$(printf '%s' "$status" | jq -r '.restartCount // empty' 2>/dev/null || true)
    [ -n "$restart" ] || die "crash $point lost controller container status"
    [ "$restart" -le "$expected_restart" ] 2>/dev/null || die "crash $point incremented restartCount more than once"
    if [ "$restart" = "$expected_restart" ] && [ "$(printf '%s' "$status" | jq -r '.lastState.terminated.exitCode // empty')" = 75 ]; then return 0; fi
    [ "$ticks" -lt "$max_ticks" ] || die "timed out waiting for exact exit 75 after $point"
    sleep 0.1; ticks=$((ticks + 1))
  done
}
assert_automation_transition() {
  expected_total=$1; source_uid=$2; source_rv=$3; label=$4
  elapsed=0
  while :; do
    ctx get automationruns -n "$NAMESPACE" -o json >"$WORK/automationruns-${label}.json"
    ctx get events.events.k8s.io -n "$NAMESPACE" -o json >"$WORK/events-${label}.json"
    ctx get oxideloopstate "$STATE_NAME" -n "$NAMESPACE" -o json >"$WORK/state-automation-${label}.json"
    if jq -e --argjson expected "$expected_total" --arg rootUid "$ROOT_UID" --arg namespace "$NAMESPACE" --arg sourceUid "$source_uid" --arg sourceRv "$source_rv" --arg bundleDigest "$BUNDLE_DIGEST" '
    .items as $runs |
    ($runs | length == $expected) and
    ($runs | all(.[]; .spec.rootUid == $rootUid and .spec.bundleDigest == $bundleDigest and .spec.program.digest == $bundleDigest and .spec.source == {apiVersion:"v1",kind:"ConfigMap",namespace:$namespace,name:"demo-input"} and .spec.sourceUid == $sourceUid and (.spec.sourceResourceVersion | type == "string") and (.spec.sourceTransitionId | test("^[0-9a-f]{64}$")) and (.spec.occurrenceId | test("^[0-9a-f]{64}$")) and (.metadata.uid | type == "string") and ([.metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}]) and .status.attempts == 1 and (.status.completion.state == "Succeeded") and (.status.materializationName | type == "string") and (.status.materializationUid | type == "string"))) and
    ([ $runs[].spec.occurrenceId ] | unique | length == $expected) and
    ([ $runs[].spec.sourceTransitionId ] | unique | length == $expected) and
    ([ $runs[].spec.sourceResourceVersion ] | unique | length == $expected) and
    ([ $runs[].metadata.uid ] | unique | length == $expected) and
    ([ $runs[].status.materializationUid ] | unique | length == $expected) and
    ([ $runs[] | select(.spec.sourceResourceVersion == $sourceRv) ] | length == (if $expected == 0 then 0 else 1 end))
  ' "$WORK/automationruns-${label}.json" >/dev/null && jq -e --slurpfile runs "$WORK/automationruns-${label}.json" --argjson expected "$expected_total" --arg namespace "$NAMESPACE" --arg rootUid "$ROOT_UID" --arg sourceUid "$source_uid" '
    [.items[] | select(.reason == "InputChanged")] as $events |
    ($events | length == $expected) and
    ($events | all(.[]; .metadata.uid | type == "string")) and
    ([ $events[].metadata.uid ] | unique | length == $expected) and
    ($runs[0].items | all(.[]; . as $run | $events | any(.[];
      .metadata.name == $run.status.materializationName and
      .metadata.uid == $run.status.materializationUid and
      .regarding == {apiVersion:"v1",kind:"ConfigMap",namespace:$namespace,name:"demo-input",uid:$sourceUid,resourceVersion:$run.spec.sourceResourceVersion} and
      .metadata.annotations["runtime.oxideloop.dev/root-uid"] == $rootUid and
      .metadata.annotations["runtime.oxideloop.dev/occurrence-id"] == $run.spec.occurrenceId and
      (.metadata.labels["runtime.oxideloop.dev/root-uid"] | test("^[0-9a-f]{63}$")) and
      (.metadata.labels["runtime.oxideloop.dev/occurrence-id"] | test("^[0-9a-f]{63}$")) and
      ([.metadata.ownerReferences[]?] == [{apiVersion:"runtime.oxideloop.dev/v1alpha1",kind:"AutomationRun",name:$run.metadata.name,uid:$run.metadata.uid,controller:true,blockOwnerDeletion:false}])
    ))) and
    ($events | all(.[]; . as $event | $runs[0].items | any(.[]; .status.materializationName == $event.metadata.name and .status.materializationUid == $event.metadata.uid)))
  ' "$WORK/events-${label}.json" >/dev/null && jq -e --slurpfile runs "$WORK/automationruns-${label}.json" --argjson expected "$expected_total" --arg namespace "$NAMESPACE" '
    (.spec.state.automation // {}) as $automation |
    ((.spec.state.pendingDependencies // []) | length == 0) and
    (.spec.state.applyTransaction == null and .spec.state.pendingPersist == null and .spec.state.pruneCandidate == null and .spec.state.generationAdvanceClosure == null and .spec.state.deletion == null) and
    (($automation.baselines // []) | length == 1) and
    (($automation.baselines // [])[0].automationId == "demo.web-application.input-changed") and
    (($automation.baselines // [])[0].source == {apiVersion:"v1",kind:"ConfigMap",namespace:$namespace,name:"demo-input"}) and
    (($automation.pending // []) | length == 0) and
    (($automation.completions // []) | length == $expected) and
    ($runs[0].items | all(.[]; . as $run | ($automation.completions // []) | any(.[];
      .occurrenceId == $run.spec.occurrenceId and .runName == $run.metadata.name and .runUid == $run.metadata.uid and
      (.oldResourceVersion | type == "string") and .oldResourceVersion != .completedResourceVersion and
      .completedResourceVersion == $run.metadata.resourceVersion and
      .materializationName == $run.status.materializationName and .materializationUid == $run.status.materializationUid and .outcome == "Succeeded"
    ))) and
    (($automation.completions // []) | all(.[]; . as $completion | $runs[0].items | any(.[]; .spec.occurrenceId == $completion.occurrenceId and .metadata.uid == $completion.runUid)))
  ' "$WORK/state-automation-${label}.json" >/dev/null; then
      return 0
    fi
    [ "$elapsed" -lt "$TIMEOUT" ] || die "$label: timed out waiting for exact source, AutomationRun, Event, and durable-completion convergence"
    sleep 2; elapsed=$((elapsed + 2))
  done
}
capture_transition() {
  label=$1
  atomic_capture "automationruns-${label}.json" ctx get automationruns -n "$NAMESPACE" -o json || die "failed to capture AutomationRuns for $label"
  atomic_capture "events-${label}.json" ctx get events.events.k8s.io -n "$NAMESPACE" -o json || die "failed to capture Events for $label"
  atomic_capture "state-automation-${label}.json" ctx get oxideloopstate "$STATE_NAME" -n "$NAMESPACE" -o json || die "failed to capture automation state for $label"
}
write_root() {
  cat >"$WORK/root.yaml" <<EOF
apiVersion: demo.oxideloop.dev/v1alpha1
kind: WebApplication
metadata:
  name: root
  namespace: $NAMESPACE
spec:
  message: hello
  expose: true
  program:
    configMapName: oxideloop-e2e-bundle
    key: bundle.archive
    digest: $BUNDLE_DIGEST
EOF
}
arm_crash() {
  point=$1; pod=$(controller_pod); CRASH_POD_NAME=$pod
  CRASH_POD_UID=$(ctx get pod -n "$NAMESPACE" "$pod" -o jsonpath='{.metadata.uid}')
  CRASH_RESTART=$(controller_status "$pod" | jq -er '.restartCount')
  ctx exec -n "$NAMESPACE" "$pod" -c controller -- /usr/local/bin/oxideloop-e2e-failpoint-control --socket /run/oxideloop/failpoints/control.sock --point "$point" | tee "$EVIDENCE/failpoint-${point}.txt"
}
await_crash() {
  point=$1
  pod=$(controller_pod)
  [ "$pod" = "$CRASH_POD_NAME" ] 2>/dev/null || die "crash $point changed controller pod name"
  wait_for_exact_crash "$point" "$CRASH_POD_NAME" "$CRASH_POD_UID" "$((CRASH_RESTART + 1))"
  atomic_capture "controller-${point}-previous.log" ctx logs -n "$NAMESPACE" "$CRASH_POD_NAME" -c controller --previous --tail=-1 || die "missing previous controller log for $point"
  ctx rollout status deployment/oxideloop-controller -n "$NAMESPACE" --timeout="${TIMEOUT}s"
}
wait_for_no_controller_pod() {
  description=$1; elapsed=0
  while :; do
    live=$(ctx get pods -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json 2>/dev/null | jq '.items | length' 2>/dev/null || printf '1')
    if [ "$live" = 0 ]; then return 0; fi
    [ "$elapsed" -lt "$TIMEOUT" ] || die "timed out waiting for $description"
    sleep 2; elapsed=$((elapsed + 2))
  done
}
wait_for_preemption_controller_rotation() {
  old_uid=$1; old_restart=$2; elapsed=0
  while :; do
    snapshot=$(ctx get pods -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json 2>/dev/null || true)
    rotation=$(printf '%s' "$snapshot" | jq -r --arg oldUid "$old_uid" --argjson oldRestart "$old_restart" '
      [.items[] | select(.metadata.deletionTimestamp == null and ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length == 1))] as $ready |
      if ($ready | length) != 1 then empty
      elif $ready[0].metadata.uid == $oldUid then
        ([$ready[0].status.containerStatuses[]? | select(.name == "controller")][0].restartCount // -1) as $restart |
        if $restart > $oldRestart then [$ready[0].metadata.uid,($restart|tostring)] | @tsv else empty end
      else [$ready[0].metadata.uid,([ $ready[0].status.containerStatuses[]? | select(.name == "controller")][0].restartCount // 0 | tostring)] | @tsv
      end
    ' 2>/dev/null || true)
    if [ -n "$rotation" ]; then return 0; fi
    [ "$elapsed" -lt "$TIMEOUT" ] || die 'timed out waiting for controller rotation after blocked root preemption'
    sleep 1; elapsed=$((elapsed + 1))
  done
}
identity_snapshot() {
  ctx get webapplications,oxideloopstates,configmaps,services -n "$NAMESPACE" -o json | jq -S '
    {root:([.items[] | select(.kind == "WebApplication" and .metadata.name == "root") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name)),
     state:([.items[] | select(.kind == "OxideLoopState") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name)),
     config:([.items[] | select(.kind == "ConfigMap" and .metadata.name == "root-config") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name)),
     service:([.items[] | select(.kind == "Service" and .metadata.name == "root-service") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name))}
  '
}
capture_empty_journal() {
  label=$1; root_uid_expected=$2; state_name_expected=$3
  atomic_capture "${label}-state.json" ctx get oxideloopstates -n "$NAMESPACE" -o json || die "failed to capture $label state"
  jq -e --arg rootUid "$root_uid_expected" --arg stateName "$state_name_expected" '
    (.items | length == 1) and (.items[0].metadata.name == $stateName) and
    (.items[0].spec.root.uid == $rootUid) and (.items[0].spec.state.inventory | length == 0) and
    ((.items[0].spec.state.pendingDependencies // []) | length == 0) and
    (.items[0].spec.state.applyTransaction == null and .items[0].spec.state.pendingPersist == null and
     .items[0].spec.state.pruneCandidate == null and .items[0].spec.state.generationAdvanceClosure == null and
     .items[0].spec.state.deletion == null) and
    ((.items[0].spec.state.automation.baselines // []) | length == 0) and
    ((.items[0].spec.state.automation.pending // []) | length == 0) and
    ((.items[0].spec.state.automation.completions // []) | length == 0) and
    ([.items[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])
  ' "$EVIDENCE/${label}-state.json" >/dev/null || die "$label did not prove an empty root-bound journal"
  atomic_capture "${label}-identities.json" identity_snapshot || die "failed to capture $label identities"
}
collision_event_stream() {
  # Runtime observability is line-oriented JSON. Every line must be a complete
  # runtime record for the expected root; silently skipping malformed or
  # foreign-root lines would make the recovery proof vacuous. The pod identity
  # is bound by the exact kubectl log source and retained on each normalized
  # evidence record.
  log_file=$1; expected_root_uid=$2; expected_pod_name=$3; expected_pod_uid=$4
  expected_foreign_uid=$5; expected_foreign_rv=$6
  : >"$WORK/collision-event-stream.tmp"
  line_number=0
  while IFS= read -r line || [ -n "$line" ]; do
    line_number=$((line_number + 1))
    [ -n "$line" ] || die "collision observability line $line_number is empty"
    printf '%s\n' "$line" | jq -Rce --arg rootUid "$expected_root_uid" --arg podName "$expected_pod_name" --arg podUid "$expected_pod_uid" --arg foreignUid "$expected_foreign_uid" --arg foreignRv "$expected_foreign_rv" '
      def nonempty: type == "string" and length > 0;
      def hash_or_null: . == null or (type == "string" and test("^[0-9a-f]{64}$"));
      fromjson as $record |
      if ($record | type) != "object" then error("record is not an object") else $record end |
      if ((keys | sort) != ["acknowledgement_class","barrier_generation","cache_epoch","collision","command_class","command_detail","component","event","fence_token_sha256","phase","root_resource_version","root_uid","state_resource_version"]) then error("record field set is not exact") else . end |
      if .component != "oxideloop-controller-runtime" then error("record component is not the runtime") else . end |
      if (.event | nonempty) then . else error("record event is invalid") end |
      if (.root_uid | nonempty) and .root_uid == $rootUid then . else error("record root UID is not the expected root") end |
      if (.root_resource_version | type) != "string" or (.state_resource_version | type) != "string" then error("root/state resource versions are not strings") else . end |
      if (.phase | nonempty) and (.command_class | nonempty) then . else error("phase or command class is invalid") end |
      if (.barrier_generation | type) != "number" or (.barrier_generation | floor) != .barrier_generation or (.barrier_generation < 0) then error("barrier generation is invalid") else . end |
      if (.cache_epoch | type) != "number" or (.cache_epoch | floor) != .cache_epoch or (.cache_epoch < 0) then error("cache epoch is invalid") else . end |
      if (.acknowledgement_class == null or (.acknowledgement_class | IN("rendered","revalidated","mutation_acknowledged","mutation_failed","other"))) then . else error("acknowledgement class is invalid") end |
      if (.fence_token_sha256 | hash_or_null) then . else error("fence token hash is invalid") end |
      if .command_detail == null then .
      elif (.command_detail | type) == "object" and ((.command_detail | keys | sort) == ["attempt_token_sha256","class","observation_token_sha256","operation_token_sha256","persist_purpose","transaction_id_sha256"]) and
        (.command_detail.class | nonempty) and
        (.command_detail.observation_token_sha256 | hash_or_null) and
        (.command_detail.operation_token_sha256 | hash_or_null) and
        (.command_detail.transaction_id_sha256 | hash_or_null) and
        (.command_detail.attempt_token_sha256 | hash_or_null) and
        (.command_detail.persist_purpose == null or (.command_detail.persist_purpose | IN("prepare","dispatch_authorization","invalidate","mutation_result","completion","automation_baseline","automation_pending","automation_completion","deletion_start","deletion_prepare","deletion_mutation_result","deletion_automation_cleanup","generation_advance_closure"))) then .
      else error("command detail is invalid") end |
      if .collision == null then .
      elif (.collision | type) == "array" and all(.collision[]; . as $target | (($target | type) == "object" and ($target | keys | sort) == ["foreign_resource_version","foreign_uid","identity"] and ($target.identity | nonempty) and ($target.foreign_uid | nonempty) and ($target.foreign_resource_version | nonempty))) then .
      else error("collision witness is invalid") end |
      . + {evidence_pod_name:$podName,evidence_pod_uid:$podUid}
    ' >>"$WORK/collision-event-stream.tmp" || die "collision observability line $line_number is malformed"
  done <"$log_file"
  [ "$line_number" -gt 0 ] || die 'collision observability log is empty'
  cat "$WORK/collision-event-stream.tmp"
}
assert_collision_recovery_events() {
  log_file=$1; mode=$2; root_uid_expected=$3; pod_name_expected=$4; pod_uid_expected=$5; foreign_uid_expected=$6; foreign_rv_expected=$7
  stream="$WORK/collision-recovery-events.jsonl"
  collision_event_stream "$log_file" "$root_uid_expected" "$pod_name_expected" "$pod_uid_expected" "$foreign_uid_expected" "$foreign_rv_expected" >"$stream"
  stream_array="$WORK/collision-recovery-events.json"
  jq -s . "$stream" >"$stream_array" || die 'collision observability stream could not be assembled'
  previous=0
  if [ "$mode" = live ]; then
    anchor_line=$(jq -nr --slurpfile stream "$stream_array" --arg foreignUid "$foreign_uid_expected" --arg foreignRv "$foreign_rv_expected" '$stream[0] | to_entries[] | select(.value.event == "watching_blocked_foreign_collision" and (.value.collision | type == "array") and any(.value.collision[]; .foreign_uid == $foreignUid and .foreign_resource_version == $foreignRv)) | .key + 1' | head -1)
    [ -n "$anchor_line" ] || die 'live collision recovery lacks the exact foreign UID/RV witness'
    previous=$((anchor_line - 1))
  else
    anchor_line=$(jq -nr --slurpfile stream "$stream_array" '$stream[0] | to_entries[] | select(.value.event == "waiting_informers") | .key + 1' | head -1)
    [ -n "$anchor_line" ] || die 'restart collision recovery lacks fresh informer admission'
    previous=$((anchor_line - 1))
  fi
  require_event() {
    event=$1; line=$(jq -nr --slurpfile stream "$stream_array" --arg event "$event" --argjson after "$previous" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == $event) | .key + 1' | head -1)
    [ -n "$line" ] || die "collision recovery log is missing ordered event: $event"
    previous=$((line - 1))
  }
  # RequestRender is the authenticated absence observation after the retained
  # delete watch. Prepare advances durable state; revalidation must then carry
  # the same observation token against Prepare's post-reduce root/state pins.
  if [ "$mode" = live ]; then
    require_event fenced_reobservation
  fi
  absence_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "effect_acknowledged" and .value.command_class == "request_render" and .value.acknowledgement_class == "rendered" and (.value.fence_token_sha256 | type == "string") and (.value.command_detail.class == "request_render") and (.value.command_detail.observation_token_sha256 | type == "string") and .value.collision == null) | .key + 1' | head -1)
  [ -n "$absence_line" ] || die 'collision recovery log is missing the authenticated absence observation'
  absence_observation=$(jq -r --argjson index "$((absence_line - 1))" '.[$index].command_detail.observation_token_sha256' "$stream_array")
  previous=$((absence_line - 1))
  prepare_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" --arg observation "$absence_observation" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "effect_acknowledged" and .value.command_class == "persist" and .value.acknowledgement_class == "other" and (.value.command_detail | type == "object") and .value.command_detail.class == "persist" and .value.command_detail.persist_purpose == "prepare" and .value.command_detail.observation_token_sha256 == $observation and (.value.command_detail.operation_token_sha256 | type == "string")) | .key + 1' | head -1)
  [ -n "$prepare_line" ] || die 'collision recovery log is missing Prepare acknowledgement'
  prepare_root_rv=$(jq -r --argjson index "$((prepare_line - 1))" '.[$index].root_resource_version' "$stream_array")
  prepare_state_rv=$(jq -r --argjson index "$((prepare_line - 1))" '.[$index].state_resource_version' "$stream_array")
  previous=$((prepare_line - 1))
  revalidation_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" --arg observation "$absence_observation" --arg rootRv "$prepare_root_rv" --arg stateRv "$prepare_state_rv" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "revalidation_completed" and .value.command_class == "request_render" and .value.acknowledgement_class == "revalidated" and (.value.fence_token_sha256 | type == "string") and .value.command_detail.observation_token_sha256 == $observation and .value.root_resource_version == $rootRv and .value.state_resource_version == $stateRv and .value.collision == null) | .key + 1' | head -1)
  [ -n "$revalidation_line" ] || die 'collision recovery log is missing the correlated revalidation completion'
  previous=$((revalidation_line - 1))
  dispatch_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "dispatch_authorization_acknowledged" and .value.command_class == "persist" and .value.acknowledgement_class == "other" and .value.command_detail.persist_purpose == "dispatch_authorization") | .key + 1' | head -1)
  [ -n "$dispatch_line" ] || die 'collision recovery log is missing dispatch authorization acknowledgement'
  previous=$((dispatch_line - 1))
  mutation_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "mutation_post_acknowledged" and .value.command_class == "mutate" and .value.acknowledgement_class == "mutation_acknowledged" and (.value.command_detail.transaction_id_sha256 | type == "string") and (.value.command_detail.attempt_token_sha256 | type == "string")) | .key + 1' | head -1)
  [ -n "$mutation_line" ] || die 'collision recovery log is missing mutation POST acknowledgement'
  previous=$((mutation_line - 1))
  result_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "mutation_result_persist_acknowledged" and .value.command_class == "persist" and .value.acknowledgement_class == "other" and .value.command_detail.persist_purpose == "mutation_result" and (.value.command_detail.operation_token_sha256 | type == "string")) | .key + 1' | head -1)
  [ -n "$result_line" ] || die 'collision recovery log is missing mutation-result persistence acknowledgement'
  previous=$((result_line - 1))
  ready_line=$(jq -nr --slurpfile stream "$stream_array" --argjson after "$previous" '$stream[0] | to_entries[] | select(.key >= $after and .value.event == "ready" and .value.phase == "Ready" and .value.command_class == "none" and .value.acknowledgement_class == null and .value.collision == null) | .key + 1' | head -1)
  [ -n "$ready_line" ] || die 'collision recovery log is missing genuine quiescent ready'
}
wait_for_blocked_collision() {
  ticks=0; max_ticks=$((TIMEOUT * 10))
  while :; do
    collision_status=$(controller_status "$COLLISION_POD" 2>/dev/null || true)
    collision_uid=$(ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
    [ "$collision_uid" = "$COLLISION_POD_UID" ] || die 'foreign collision replaced the controller pod'
    collision_ready=$(printf '%s' "$collision_status" | jq -r '.ready // false')
    [ "$collision_ready" = true ] || die 'foreign collision controller was not running and ready'
    collision_restarts=$(printf '%s' "$collision_status" | jq -r '.restartCount // 0')
    [ "$collision_restarts" = "$COLLISION_RESTART" ] || die 'foreign collision incremented controller restartCount or restarted the controller'
    ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 >"$WORK/foreign-collision-blocked.log" 2>&1 || true
    collision_event_stream "$WORK/foreign-collision-blocked.log" "$ROOT_UID" "$COLLISION_POD" "$COLLISION_POD_UID" "$FOREIGN_UID" "$FOREIGN_RV" >"$WORK/foreign-collision-blocked-events.jsonl" 2>/dev/null || true
    if jq -s -e --arg foreignUid "$FOREIGN_UID" --arg foreignRv "$FOREIGN_RV" 'any(.[]; .event == "watching_blocked_foreign_collision" and (.collision | type == "array") and any(.collision[]; .foreign_uid == $foreignUid and .foreign_resource_version == $foreignRv))' "$WORK/collision-event-stream.tmp" >/dev/null 2>&1; then
      return 0
    fi
    [ "$ticks" -lt "$max_ticks" ] || die 'timed out waiting for the blocked foreign collision watch state'
    sleep 0.1; ticks=$((ticks + 1))
  done
}
assert_blocked_collision_quiescent() {
  first_log=$1; second_log=$2
  cmp -s "$first_log" "$second_log" || die 'blocked foreign collision controller log grew or polled repeatedly'
}
canonical_blocked_endpoint() {
  endpoint_kind=$2
  jq -e -S --arg expectedKind "$endpoint_kind" '
    if (.apiVersion | type) != "string" or (.apiVersion | length) == 0 then error("blocked endpoint apiVersion is invalid")
    elif .kind != $expectedKind then error("blocked endpoint kind is invalid")
    elif (.metadata | type) != "object" then error("blocked endpoint metadata is missing")
    elif (.metadata.name | type) != "string" or (.metadata.name | length) == 0 then error("blocked endpoint name is invalid")
    elif (.metadata.uid | type) != "string" or (.metadata.uid | length) == 0 then error("blocked endpoint UID is invalid")
    elif (.metadata.resourceVersion | type) != "string" or (.metadata.resourceVersion | length) == 0 then error("blocked endpoint resourceVersion is invalid")
    elif (.metadata.generation | type) != "number" or (.metadata.generation | floor) != .metadata.generation or .metadata.generation < 1 then error("blocked endpoint generation is invalid")
    elif (.spec | type) != "object" then error("blocked endpoint spec is invalid")
    elif $expectedKind == "WebApplication" and (.status | type) != "object" then error("WebApplication status is invalid")
    elif $expectedKind == "OxideLoopState" and (.status? != null) then error("OxideLoopState status must be absent or null")
    else {metadata:{uid:.metadata.uid,resourceVersion:.metadata.resourceVersion,generation:.metadata.generation},spec:.spec,status:(.status // null)}
    end
  ' "$1"
}
container_restart_snapshot() {
  jq -e -S '
    if (.metadata.uid | type) != "string" or (.metadata.uid | length) == 0 then error("controller pod UID is invalid")
    elif (.spec.containers | type) != "array" then error("declared container list is invalid")
    elif ([.spec.containers[] | .name] | sort) != ["controller","renderer"] then error("declared container set is not exactly controller+renderer")
    elif (.status.containerStatuses | type) != "array" then error("live container status list is invalid")
    else
      ([.status.containerStatuses[] | {name,restartCount}] | sort_by(.name)) as $containers |
      ([.spec.containers[] | .name] | sort) as $declared |
      ([.spec.initContainers[]? | .name] | sort) as $init_declared |
      ([.status.initContainerStatuses[]? | {name,restartCount}] | sort_by(.name)) as $init_containers |
      if ([$containers[].name] | sort) != $declared then error("live container status set does not match declared containers")
      elif (any($containers[]; (.name | type) != "string" or (.name | length) == 0 or (.restartCount | type) != "number" or (.restartCount | floor) != .restartCount or .restartCount < 0)) then error("live container restart count is invalid")
      elif ([$init_containers[].name] | sort) != $init_declared then error("init container status set does not match declared init containers")
      elif (any($init_containers[]; (.name | type) != "string" or (.name | length) == 0 or (.restartCount | type) != "number" or (.restartCount | floor) != .restartCount or .restartCount < 0)) then error("init container restart count is invalid")
      else {uid:.metadata.uid,containers:$containers,initContainers:$init_containers}
      end
    end
  ' "$1"
}
assert_container_restart_snapshot_stable() {
  before_snapshot=$(container_restart_snapshot "$1") || die 'blocked interval before container restart snapshot is invalid'
  after_snapshot=$(container_restart_snapshot "$2") || die 'blocked interval after container restart snapshot is invalid'
  jq -ne --argjson before "$before_snapshot" --argjson after "$after_snapshot" '$before == $after' >/dev/null || die 'controller or renderer restart count changed during blocked interval/live recovery'
}
assert_collision_blocked_interval() {
  prefix=${1:-collision-blocked}
  for object in root state; do
    endpoint_kind=OxideLoopState; [ "$object" = root ] && endpoint_kind=WebApplication
    canonical_blocked_endpoint "$EVIDENCE/${prefix}-${object}-before.json" "$endpoint_kind" >"$WORK/${prefix}-${object}-before.canonical.json" || die "blocked collision ${object} before endpoint metadata is invalid"
    canonical_blocked_endpoint "$EVIDENCE/${prefix}-${object}-after.json" "$endpoint_kind" >"$WORK/${prefix}-${object}-after.canonical.json" || die "blocked collision ${object} after endpoint metadata is invalid"
    cmp -s "$WORK/${prefix}-${object}-before.canonical.json" "$WORK/${prefix}-${object}-after.canonical.json" || die "blocked collision ${object} spec/status or resourceVersion changed"
  done
  cmp -s "$EVIDENCE/${prefix}-foreign-before.json" "$EVIDENCE/${prefix}-foreign-after.json" || die 'blocked collision foreign ConfigMap bytes changed'
  cmp -s "$EVIDENCE/${prefix}-log-before.log" "$EVIDENCE/${prefix}-log-after.log" || die 'blocked foreign collision controller log grew or polled repeatedly'
  assert_container_restart_snapshot_stable "$EVIDENCE/${prefix}-pod-before.json" "$EVIDENCE/${prefix}-pod-after.json"
}
wait_for_collision_recovery_quiescence() {
  wait_json 'collision recovery recreated ConfigMap ownership' '(.metadata.uid | type == "string") and ([.metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])' configmap/root-config --arg rootUid "$ROOT_UID"
  wait_json 'collision recovery state quiescence' '(.spec.state.inventory | length == 2) and ((.spec.state.pendingDependencies // []) | length == 0) and (.spec.state.applyTransaction == null) and (.spec.state.pendingPersist == null) and (.spec.state.pruneCandidate == null) and (.spec.state.generationAdvanceClosure == null) and (.spec.state.deletion == null) and ((.spec.state.automation.pending // []) | length == 0)' "oxideloopstate/$COLLISION_STATE_NAME"
  wait_json 'collision recovery root quiescence' '(.status.state.rootUid == .metadata.uid) and (.status.state.name == $stateName) and (.status.state.uid | type == "string") and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest) and (.status.inputToken | type == "string") and (.status.cacheEpoch | type == "number")' webapplication/root --arg stateName "$COLLISION_STATE_NAME"
}
wait_for_prepared_not_issued() {
  ticks=0; max_ticks=$((TIMEOUT * 10))
  while :; do
    snapshot="$WORK/prepared-not-issued-state.json"
    ctx get oxideloopstate "$STATE_NAME" -n "$NAMESPACE" -o json >"$snapshot" 2>/dev/null || true
    if jq -e '
      (.spec.state.pendingPersist.purpose == "Prepare") and
      (.spec.state.pendingPersist.phase == "Prepare") and
      (.spec.state.pendingPersist.token | type == "string" and test("^[0-9a-f]{64}$")) and
      (.spec.state.applyTransaction.results | length == 0) and
      (.spec.state.applyTransaction.nextCommandLifecycle.state == "PreparedNotIssued") and
      (.spec.state.applyTransaction.nextCommandLifecycle.commandId | type == "string" and test("^[0-9a-f]{64}$"))
    ' "$snapshot" >/dev/null 2>&1; then
      atomic_capture "prepared-not-issued-state.json" cp "$snapshot" || die 'failed to capture PreparedNotIssued state'
      return 0
    fi
    [ "$ticks" -lt "$max_ticks" ] || die 'timed out waiting for durable PreparedNotIssued state'
    sleep 0.1; ticks=$((ticks + 1))
  done
}

note "run=$RUN_ID cluster=$CLUSTER_NAME revision=$HEAD collisionMode=$COLLISION_MODE"
capture_collision_regression_boundary
kind create cluster --name "$CLUSTER_NAME" --image "$NODE_IMAGE" --config deploy/e2e/kind-config.yaml --wait "${TIMEOUT}s"
CLUSTER_CREATED=true
ACTUAL_NODE_IMAGE=$(docker inspect "${CLUSTER_NAME}-control-plane" --format '{{.Config.Image}}')
[ "$ACTUAL_NODE_IMAGE" = "$NODE_IMAGE" ] || die "kind control-plane image differs from the immutable requested image: $ACTUAL_NODE_IMAGE"
if [ "${OXIDELOOP_E2E_SELF_TEST_STOP_AFTER_CLUSTER_CREATE:-0}" = 1 ]; then
  die 'self-test stop after cluster creation'
fi
if [ "${OXIDELOOP_E2E_SELF_TEST_FINISH_AFTER_CLUSTER_CREATE:-0}" = 1 ]; then
  exit 0
fi
ctx create namespace "$NAMESPACE"
ctx apply -f deploy/crds/oxideloop-crds.yaml
ctx wait --for=condition=Established crd/webapplications.demo.oxideloop.dev --timeout="${TIMEOUT}s"

# Build from this exact source and retain the archive plus immutable metadata.
pnpm install --frozen-lockfile
pnpm bundle:e2e -- --out "$WORK/bundle.archive" --metadata "$WORK/bundle-metadata.json"
BUNDLE_DIGEST=$(jq -er '.buildBundle.bundleDigest' "$WORK/bundle-metadata.json")
BUNDLE_SHA=$(jq -er '.buildBundle.archive.sha256' "$WORK/bundle-metadata.json")
test "$(hash_file "$WORK/bundle.archive")" = "$BUNDLE_SHA" || die "bundle archive digest differs from metadata"
base64 <"$WORK/bundle.archive" | tr -d '\n' >"$WORK/bundle.base64"
cat >"$WORK/bundle-configmap.yaml" <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: oxideloop-e2e-bundle
  namespace: $NAMESPACE
binaryData:
  bundle.archive: $(cat "$WORK/bundle.base64")
EOF
ctx apply -f "$WORK/bundle-configmap.yaml"

PRODUCTION_IMAGE="oxideloop-e2e:production-$RUN_ID"
FAILPOINT_IMAGE="oxideloop-e2e:failpoints-$RUN_ID"
# Disable provenance attestations so each local tag resolves to one platform
# manifest. Docker inspect then exposes both that manifest digest and its
# config digest; Kubernetes reports the latter in container status.
docker build --provenance=false --target production -t "$PRODUCTION_IMAGE" .
docker build --provenance=false --target e2e-failpoints -t "$FAILPOINT_IMAGE" .
atomic_capture docker-image-production.inspect.json docker image inspect "$PRODUCTION_IMAGE" || die 'failed to capture production Docker inspect JSON'
atomic_capture docker-image-failpoints.inspect.json docker image inspect "$FAILPOINT_IMAGE" || die 'failed to capture failpoint Docker inspect JSON'
image_identity_from_inspect() {
  tag=$1; inspect_file=$2
  jq -ec --arg tag "$tag" '
    def digest($field):
      if type == "string" and test("^sha256:[0-9a-f]{64}$") then . else error($field + " must be an exact lowercase sha256 digest") end;
    def variant($field):
      if . == null then null elif type == "string" and length > 0 then . else error($field + " must be null or a nonempty string") end;
    def platform($field):
      (if has("variant") then .variant else null end) as $variant |
      if type == "object" and (.os | type == "string" and length > 0) and (.architecture | type == "string" and length > 0) then {os:.os,architecture:.architecture,variant:($variant | variant($field + ".variant"))} else error($field + " must include os and architecture") end;
    def single_platform_manifest($field):
      if . == "application/vnd.docker.distribution.manifest.v2+json" or . == "application/vnd.oci.image.manifest.v1+json" then . else error($field + " must be a Docker v2 or OCI single-platform image manifest") end;
    def positive_integer($field):
      if type == "number" and . > 0 and floor == . then . else error($field + " must be a positive integer") end;
    if type != "array" or length != 1 then error("Docker inspect must return exactly one image") else .[0] end |
    . as $image |
    (if $image | has("RepoTags") then $image.RepoTags else [] end) as $repo_tags |
    if ($repo_tags | type != "array" or index($tag) == null) then error("Docker inspect does not retain the exact requested tag") else . end |
    ($image.Id | digest("Docker inspect Id")) as $id |
    (if $image | has("Descriptor") then $image.Descriptor else null end) as $descriptor |
    if $descriptor == null or ($descriptor | type == "object") then . else error("Docker inspect Descriptor must be an object or null") end |
    ($image.Os | if type == "string" and length > 0 then . else error("Docker inspect Os is missing") end) as $os |
    ($image.Architecture | if type == "string" and length > 0 then . else error("Docker inspect Architecture is missing") end) as $architecture |
    (($image | if has("Variant") then .Variant else null end) | variant("Docker inspect Variant")) as $inspect_variant |
    ({os:$os,architecture:$architecture,variant:$inspect_variant}) as $inspect_platform |
    (if $descriptor == null then null else ($descriptor.digest | digest("Descriptor.digest")) end) as $manifest |
    (if $descriptor == null then null else ($descriptor.mediaType | single_platform_manifest("Descriptor.mediaType")) end) as $manifest_media_type |
    (if $descriptor == null then null else ($descriptor.size | positive_integer("Descriptor.size")) end) as $manifest_size |
    (if $descriptor == null then null elif ($descriptor | has("platform")) then $descriptor.platform else null end) as $descriptor_platform |
    (if $descriptor_platform == null then $inspect_platform else ($descriptor_platform | platform("Descriptor.platform")) end) as $platform |
    if $platform != $inspect_platform then error("Descriptor platform differs from Docker inspect platform") else . end |
    (if $descriptor == null then null elif ($descriptor | has("annotations")) then $descriptor.annotations else null end) as $annotations |
    if $annotations == null or ($annotations | type == "object") then . else error("Descriptor.annotations must be an object or null") end |
    (if $annotations == null then null elif ($annotations | has("config.digest")) then $annotations["config.digest"] else null end) as $annotated_config |
    if ($descriptor != null and $annotated_config != null and $id != $manifest) then error("Docker inspect Id and Descriptor.digest disagree") else . end |
    (if $annotated_config != null then {digest:($annotated_config | digest("Descriptor.annotations.config.digest")),source:"Descriptor.annotations.config.digest"}
     elif $descriptor == null then {digest:$id,source:"Id (legacy descriptor unavailable)"}
     elif $id != $manifest then {digest:$id,source:"Id (descriptor annotation unavailable)"}
     else error("Docker inspect cannot prove a config digest distinct from its manifest digest") end) as $config |
    if ($manifest != null and $config.digest == $manifest) then error("Docker config and manifest digests must be distinct") else . end |
    {tag:$tag,inspectId:$id,manifestDigest:$manifest,manifestMediaType:$manifest_media_type,manifestSize:$manifest_size,configDigest:$config.digest,configDigestSource:$config.source,platform:$platform}
  ' "$inspect_file"
}
PRODUCTION_IMAGE_IDENTITY=$(image_identity_from_inspect "$PRODUCTION_IMAGE" "$EVIDENCE/docker-image-production.inspect.json") || die 'production Docker inspect identity is invalid'
FAILPOINT_IMAGE_IDENTITY=$(image_identity_from_inspect "$FAILPOINT_IMAGE" "$EVIDENCE/docker-image-failpoints.inspect.json") || die 'failpoint Docker inspect identity is invalid'
PRODUCTION_CONFIG_DIGEST=$(printf '%s' "$PRODUCTION_IMAGE_IDENTITY" | jq -er '.configDigest')
FAILPOINT_CONFIG_DIGEST=$(printf '%s' "$FAILPOINT_IMAGE_IDENTITY" | jq -er '.configDigest')
kind load docker-image --name "$CLUSTER_NAME" "$PRODUCTION_IMAGE" "$FAILPOINT_IMAGE"
kubectl kustomize deploy/e2e | sed -e "s#ghcr.io/oxideloop/oxideloop:production#$PRODUCTION_IMAGE#g" -e "s#ghcr.io/oxideloop/oxideloop:e2e-failpoints#$FAILPOINT_IMAGE#g" | ctx apply -n "$NAMESPACE" -f -
ctx rollout status deployment/oxideloop-controller -n "$NAMESPACE" --timeout="${TIMEOUT}s"
pod_image_identities_from_json() {
  pod_file=$1; controller_image=$2; renderer_image=$3; controller_config=$4; renderer_config=$5
  jq -ec --arg controllerImage "$controller_image" --arg rendererImage "$renderer_image" --arg controllerConfig "$controller_config" --arg rendererConfig "$renderer_config" '
  def local_image_tag($field):
    if type == "string" and test("^[a-z0-9][a-z0-9._-]*:[a-z0-9][a-z0-9._-]*$") then . else error($field + " must be an unqualified local image tag") end;
  def status_image($expected):
    if . == $expected then {observedImage:.,normalizedImage:$expected,relation:"exact"}
    elif . == ("docker.io/library/" + $expected) then {observedImage:.,normalizedImage:$expected,relation:"docker.io/library-qualified"}
    else error("container status image is not the exact local tag or its docker.io/library qualification") end;
  def runtime_digest:
    if type != "string" then error("container imageID is missing") else sub("^[A-Za-z0-9+.-]+://"; "") end |
    if test("^sha256:[0-9a-f]{64}$") then .
    elif test("^[^@]+@sha256:[0-9a-f]{64}$") then sub("^[^@]+@"; "")
    else error("container imageID is not an exact sha256 digest") end;
  [.items[] | select(.metadata.deletionTimestamp == null)] as $pods |
  if ($pods | length) != 1 then error("expected exactly one live controller pod") else $pods[0] end |
  . as $pod |
  ($controllerImage | local_image_tag("controller expected image")) as $controller_short |
  ($rendererImage | local_image_tag("renderer expected image")) as $renderer_short |
  (["controller", "renderer"] | map(. as $name |
    (if $name == "controller" then $controller_short else $renderer_short end) as $expected |
    ([$pod.spec.containers[] | select(.name == $name) | .image] | if length == 1 then .[0] else error("controller container declaration is ambiguous") end) as $declared |
    ([$pod.status.containerStatuses[] | select(.name == $name) | .image] | if length == 1 then .[0] else error("controller container status is ambiguous") end | status_image($expected)) as $status |
    {name:$name,declaredImage:$declared,observedImage:$status.observedImage,normalizedImage:$status.normalizedImage,imageRelation:$status.relation,configDigest:([$pod.status.containerStatuses[] | select(.name == $name) | .imageID | runtime_digest] | if length == 1 then .[0] else error("controller container imageID is ambiguous") end)}
  )) as $observed |
  ([ $observed[] | {name,declaredImage,normalizedImage,configDigest} ]) as $normalized |
  if $normalized != [{name:"controller",declaredImage:$controller_short,normalizedImage:$controller_short,configDigest:$controllerConfig},{name:"renderer",declaredImage:$renderer_short,normalizedImage:$renderer_short,configDigest:$rendererConfig}] then error("controller and renderer runtime config digests differ from their exact Docker inspect identities")
  else {pod:{name:$pod.metadata.name,uid:$pod.metadata.uid},expected:{controller:{tag:$controller_short,acceptedStatusImages:[$controller_short,"docker.io/library/" + $controller_short],configDigest:$controllerConfig},renderer:{tag:$renderer_short,acceptedStatusImages:[$renderer_short,"docker.io/library/" + $renderer_short],configDigest:$rendererConfig}},observed:$observed} end
  ' "$pod_file"
}
atomic_capture controller-pod-images.json ctx get pod -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json || die 'failed to capture controller pod image JSON'
POD_IMAGE_IDENTITIES=$(pod_image_identities_from_json "$EVIDENCE/controller-pod-images.json" "$FAILPOINT_IMAGE" "$PRODUCTION_IMAGE" "$FAILPOINT_CONFIG_DIGEST" "$PRODUCTION_CONFIG_DIGEST") || die 'controller and renderer image identity proof failed'
jq -n --arg node "$NODE_IMAGE" --argjson production "$PRODUCTION_IMAGE_IDENTITY" --argjson failpoints "$FAILPOINT_IMAGE_IDENTITY" --argjson pod "$POD_IMAGE_IDENTITIES" '{nodeImage:$node,production:$production,failpoints:$failpoints,podIdentity:$pod}' >"$EVIDENCE/image-ids.json"

# Foreign desired-name collision proves default-deny adoption before bootstrap.
# The blocked phase is intentionally quiescent: it owns no effect and waits
# only for the retained desired-object watch. Recovery is then exercised in
# either the same pod (live) or after a deliberate controller restart.
ctx create configmap root-config -n "$NAMESPACE" --from-literal=foreign=true
atomic_capture foreign-before.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing foreign collision object evidence'
FOREIGN_UID=$(jq -r '.metadata.uid' "$EVIDENCE/foreign-before.json")
FOREIGN_RV=$(jq -r '.metadata.resourceVersion' "$EVIDENCE/foreign-before.json")
COLLISION_POD=$(controller_pod)
COLLISION_POD_UID=$(ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o jsonpath='{.metadata.uid}')
COLLISION_RESTART=$(controller_status "$COLLISION_POD" | jq -er '.restartCount')
write_root; ctx apply -f "$WORK/root.yaml"
ROOT_UID=$(root_uid)
wait_for_blocked_collision
atomic_capture foreign-after.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing foreign collision after evidence'
jq -e --slurpfile before "$EVIDENCE/foreign-before.json" '. == $before[0]' "$EVIDENCE/foreign-after.json" >/dev/null || die 'foreign collision object was mutated in at least one field'
cmp -s "$EVIDENCE/foreign-before.json" "$EVIDENCE/foreign-after.json" || die 'foreign collision object bytes changed'
atomic_capture foreign-collision-root.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing foreign collision root evidence'
COLLISION_STATE_NAME=$(jq -er '.status.state.name' "$EVIDENCE/foreign-collision-root.json")
capture_empty_journal foreign-collision "$ROOT_UID" "$COLLISION_STATE_NAME"
assert_count "$(ctx get service -n "$NAMESPACE" -o json | jq '.items | length')" 0 'foreign collision service claim'
assert_count "$(ctx get automationruns -n "$NAMESPACE" -o json | jq '.items | length')" 0 'foreign collision AutomationRun claim'
atomic_capture collision-blocked-root-before.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing blocked collision root before evidence'
atomic_capture collision-blocked-state-before.json ctx get oxideloopstate "$COLLISION_STATE_NAME" -n "$NAMESPACE" -o json || die 'missing blocked collision state before evidence'
atomic_capture collision-blocked-foreign-before.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing blocked collision foreign object before evidence'
atomic_capture collision-blocked-pod-before.json ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o json || die 'missing blocked collision pod before evidence'
atomic_capture collision-blocked-log-before.log ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 || die 'missing blocked foreign collision controller log'
collision_event_stream "$EVIDENCE/collision-blocked-log-before.log" "$ROOT_UID" "$COLLISION_POD" "$COLLISION_POD_UID" "$FOREIGN_UID" "$FOREIGN_RV" >"$EVIDENCE/collision-blocked-events.jsonl"
sleep 3
atomic_capture collision-blocked-root-after.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing blocked collision root after evidence'
atomic_capture collision-blocked-state-after.json ctx get oxideloopstate "$COLLISION_STATE_NAME" -n "$NAMESPACE" -o json || die 'missing blocked collision state after evidence'
atomic_capture collision-blocked-foreign-after.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing blocked collision foreign object after evidence'
atomic_capture collision-blocked-pod-after.json ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o json || die 'missing blocked collision pod after evidence'
atomic_capture collision-blocked-log-after.log ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 || die 'missing blocked foreign collision quiescence log'
assert_collision_blocked_interval

# Root deletion is an explicit preemption boundary while the desired child is
# blocked. The unowned same-name sentinel must survive byte-for-byte; the root
# is then recreated and a new blocked baseline is established before the
# selected recovery mode runs.
atomic_capture collision-preemption-root-before.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing blocked root preemption before evidence'
atomic_capture collision-preemption-foreign-before.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing blocked root preemption foreign before evidence'
PREEMPTION_POD=$(controller_pod)
PREEMPTION_POD_UID=$(ctx get pod -n "$NAMESPACE" "$PREEMPTION_POD" -o jsonpath='{.metadata.uid}')
PREEMPTION_RESTART=$(controller_status "$PREEMPTION_POD" | jq -er '.restartCount')
ctx delete webapplication root -n "$NAMESPACE" --wait=false
wait_json 'blocked root deletion preemption root absence' '.items | length == 0' webapplications
atomic_capture collision-preemption-foreign-after.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing blocked root preemption foreign after evidence'
cmp -s "$EVIDENCE/collision-preemption-foreign-before.json" "$EVIDENCE/collision-preemption-foreign-after.json" || die 'blocked root deletion preemption mutated or deleted the foreign ConfigMap'
wait_for_preemption_controller_rotation "$PREEMPTION_POD_UID" "$PREEMPTION_RESTART"

write_root; ctx apply -f "$WORK/root.yaml"
ROOT_UID=$(root_uid)
COLLISION_POD=$(controller_pod)
COLLISION_POD_UID=$(ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o jsonpath='{.metadata.uid}')
COLLISION_RESTART=$(controller_status "$COLLISION_POD" | jq -er '.restartCount')
wait_for_blocked_collision
atomic_capture collision-recreated-blocked-root-before.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing recreated blocked root before evidence'
COLLISION_STATE_NAME=$(jq -er '.status.state.name' "$EVIDENCE/collision-recreated-blocked-root-before.json")
capture_empty_journal collision-recreated-blocked "$ROOT_UID" "$COLLISION_STATE_NAME"
assert_count "$(ctx get service -n "$NAMESPACE" -o json | jq '.items | length')" 0 'recreated foreign collision service claim'
assert_count "$(ctx get automationruns -n "$NAMESPACE" -o json | jq '.items | length')" 0 'recreated foreign collision AutomationRun claim'
atomic_capture collision-recreated-blocked-state-before.json ctx get oxideloopstate "$COLLISION_STATE_NAME" -n "$NAMESPACE" -o json || die 'missing recreated blocked state before evidence'
atomic_capture collision-recreated-blocked-foreign-before.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing recreated blocked foreign before evidence'
atomic_capture collision-recreated-blocked-pod-before.json ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o json || die 'missing recreated blocked pod before evidence'
atomic_capture collision-recreated-blocked-log-before.log ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 || die 'missing recreated blocked controller log'
collision_event_stream "$EVIDENCE/collision-recreated-blocked-log-before.log" "$ROOT_UID" "$COLLISION_POD" "$COLLISION_POD_UID" "$FOREIGN_UID" "$FOREIGN_RV" >"$EVIDENCE/collision-recreated-blocked-events.jsonl"
sleep 3
atomic_capture collision-recreated-blocked-root-after.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing recreated blocked root after evidence'
atomic_capture collision-recreated-blocked-state-after.json ctx get oxideloopstate "$COLLISION_STATE_NAME" -n "$NAMESPACE" -o json || die 'missing recreated blocked state after evidence'
atomic_capture collision-recreated-blocked-foreign-after.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing recreated blocked foreign after evidence'
atomic_capture collision-recreated-blocked-pod-after.json ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o json || die 'missing recreated blocked pod after evidence'
atomic_capture collision-recreated-blocked-log-after.log ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 || die 'missing recreated blocked controller after log'
assert_collision_blocked_interval collision-recreated-blocked

recover_collision_restart() {
  # Restart mode proves that a process-local collision witness is not replay
  # authority. Delete the controller before deleting the foreign object, then
  # allow fresh informer admission to recover without a root annotation.
  ctx scale deployment/oxideloop-controller -n "$NAMESPACE" --replicas=0
  wait_for_no_controller_pod 'controller deployment scale-to-zero'
  atomic_capture controller-scaled-zero-pods.json ctx get pods -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json || die 'failed to capture scale-to-zero controller pod evidence'
  jq -e '.items | length == 0' "$EVIDENCE/controller-scaled-zero-pods.json" >/dev/null || die 'a controller Pod remained after scale-to-zero'
  capture_empty_journal foreign-collision-stopped "$ROOT_UID" "$COLLISION_STATE_NAME"
  cmp -s "$EVIDENCE/collision-recreated-blocked-identities.json" "$EVIDENCE/foreign-collision-stopped-identities.json" || die 'stopped collision identity snapshot changed root, state, or foreign ConfigMap UID'
  atomic_capture foreign-collision-stopped.json ctx get configmap root-config -n "$NAMESPACE" -o json || die 'missing stopped foreign collision object evidence'
  jq -e --slurpfile before "$EVIDENCE/foreign-before.json" '. == $before[0]' "$EVIDENCE/foreign-collision-stopped.json" >/dev/null || die 'stopped foreign collision object is not byte-for-byte unchanged'
  cmp -s "$EVIDENCE/foreign-before.json" "$EVIDENCE/foreign-collision-stopped.json" || die 'stopped foreign collision object bytes changed'
  atomic_capture foreign-collision-stopped-root.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing stopped foreign collision root evidence'
  jq -e --arg rootUid "$ROOT_UID" '.metadata.uid == $rootUid' "$EVIDENCE/foreign-collision-stopped-root.json" >/dev/null || die 'stopped collision root UID changed'
  ctx delete configmap root-config -n "$NAMESPACE"
  atomic_capture foreign-collision-recovery-root.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing stopped collision recovery root evidence'
  COLLISION_STATE_UID=$(jq -er '.status.state.uid' "$EVIDENCE/foreign-collision-recovery-root.json")
  jq -e --arg rootUid "$ROOT_UID" --arg stateName "$COLLISION_STATE_NAME" --arg stateUid "$COLLISION_STATE_UID" '
    (.metadata.uid == $rootUid) and
    (.status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and
    (.status.state.rootUid == .metadata.uid)
  ' "$EVIDENCE/foreign-collision-recovery-root.json" >/dev/null || die 'stopped collision recovery root link or UID is not exact'
  capture_empty_journal foreign-collision-recovery "$ROOT_UID" "$COLLISION_STATE_NAME"
  jq -e --slurpfile recreated "$EVIDENCE/collision-recreated-blocked-identities.json" '
    (.root == $recreated[0].root) and (.state == $recreated[0].state) and
    (.config == []) and (.service == [])
  ' "$EVIDENCE/foreign-collision-recovery-identities.json" >/dev/null || die 'stopped collision recovery identities or foreign ConfigMap absence is not exact'
  ctx scale deployment/oxideloop-controller -n "$NAMESPACE" --replicas=1
  ctx rollout status deployment/oxideloop-controller -n "$NAMESPACE" --timeout="${TIMEOUT}s"
  COLLISION_POD=$(controller_pod)
  RECOVERY_POD_UID=$(ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o jsonpath='{.metadata.uid}')
  RECOVERY_RESTART=$(controller_status "$COLLISION_POD" | jq -er '.restartCount')
  wait_for_collision_recovery_quiescence
  atomic_capture controller-foreign-collision-recovery.log ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 || die 'missing restarted collision recovery controller log'
  assert_collision_recovery_events "$EVIDENCE/controller-foreign-collision-recovery.log" "$COLLISION_MODE" "$ROOT_UID" "$COLLISION_POD" "$RECOVERY_POD_UID" "$FOREIGN_UID" "$FOREIGN_RV"
}
recover_collision_live() {
  # Live mode leaves the blocked controller in place. A delete watch alone
  # must retire the witness and converge in the same pod without annotation.
  atomic_capture foreign-collision-live-root.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing live collision root evidence'
  atomic_capture foreign-collision-live-identities.json identity_snapshot || die 'missing live collision identity evidence'
  atomic_capture foreign-collision-live-pod-before.json ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o json || die 'missing live collision pod-before evidence'
  ctx delete configmap root-config -n "$NAMESPACE"
  atomic_capture foreign-collision-live-delete-root.json ctx get webapplication root -n "$NAMESPACE" -o json || die 'missing live collision delete root evidence'
  jq -e --arg rootUid "$ROOT_UID" '(.metadata.uid == $rootUid)' "$EVIDENCE/foreign-collision-live-delete-root.json" >/dev/null || die 'live collision delete changed the root'
  wait_json 'recreated root ConfigMap ownership' '(.metadata.uid | type == "string") and ([.metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])' configmap/root-config --arg rootUid "$ROOT_UID"
  RECOVERED_CONFIG_UID=$(ctx get configmap root-config -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')
  [ "$RECOVERED_CONFIG_UID" != "$FOREIGN_UID" ] || die 'collision recovery reused the foreign ConfigMap UID'
  RECOVERY_POD_UID=$(ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o jsonpath='{.metadata.uid}')
  RECOVERY_RESTART=$(controller_status "$COLLISION_POD" | jq -er '.restartCount')
  [ "$RECOVERY_POD_UID" = "$COLLISION_POD_UID" ] || die 'live collision recovery replaced the controller pod'
  [ "$RECOVERY_RESTART" = "$COLLISION_RESTART" ] || die 'live collision recovery changed controller restartCount'
  wait_for_collision_recovery_quiescence
  atomic_capture foreign-collision-live-pod-after.json ctx get pod -n "$NAMESPACE" "$COLLISION_POD" -o json || die 'missing live collision pod-after evidence'
  assert_container_restart_snapshot_stable "$EVIDENCE/foreign-collision-live-pod-before.json" "$EVIDENCE/foreign-collision-live-pod-after.json"
  atomic_capture controller-foreign-collision-recovery.log ctx logs -n "$NAMESPACE" "$COLLISION_POD" -c controller --tail=-1 || die 'missing live collision recovery controller log'
  assert_collision_recovery_events "$EVIDENCE/controller-foreign-collision-recovery.log" "$COLLISION_MODE" "$ROOT_UID" "$COLLISION_POD" "$RECOVERY_POD_UID" "$FOREIGN_UID" "$FOREIGN_RV"
}
case "$COLLISION_MODE" in
  restart) recover_collision_restart ;;
  live) recover_collision_live ;;
  *) die 'E2E_COLLISION_MODE must be live or restart' ;;
esac
wait_json 'recreated root ConfigMap ownership' '(.metadata.uid | type == "string") and ([.metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])' configmap/root-config --arg rootUid "$ROOT_UID"
RECOVERED_CONFIG_UID=$(ctx get configmap root-config -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')
[ "$RECOVERED_CONFIG_UID" != "$FOREIGN_UID" ] || die 'collision recovery reused the foreign ConfigMap UID'
wait_json 'root current render status after ConfigMap ownership' '(.status.state.rootUid == .metadata.uid) and (.status.state.name | type == "string") and (.status.state.uid | type == "string") and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest) and (.status.inputToken | type == "string") and (.status.cacheEpoch | type == "number")' webapplication/root
test "$(root_uid)" = "$ROOT_UID" || die 'collision recovery changed the root identity'
assert_count "$(ctx get configmap root-config -n "$NAMESPACE" -o json | jq --arg uid "$ROOT_UID" '[.metadata.ownerReferences[]? | select(.uid == $uid)] | length')" 1 'managed ConfigMap owner reference'
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/initial-root.json"
STATE_NAME=$(jq -er '.status.state.name' "$EVIDENCE/initial-root.json")
wait_json 'initial state final convergence' '(.spec.state.inventory | length == 2) and ((.spec.state.pendingDependencies // []) | length == 0) and (.spec.state.applyTransaction == null) and (.spec.state.pendingPersist == null) and (.spec.state.pruneCandidate == null) and (.spec.state.generationAdvanceClosure == null) and (.spec.state.deletion == null) and ((.spec.state.automation.pending // []) | length == 0)' "oxideloopstate/$STATE_NAME"
wait_json 'initial root current render status' '(.status.state.rootUid == .metadata.uid) and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest) and (.status.inputToken | type == "string") and (.status.cacheEpoch | type == "number")' webapplication/root
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/initial-root.json"
STATE_UID=$(jq -er '.status.state.uid' "$EVIDENCE/initial-root.json")
ctx get oxideloopstate "$STATE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE/initial-state.json"
ctx get configmap root-config -n "$NAMESPACE" -o json >"$EVIDENCE/initial-config.json"
ctx get service root-service -n "$NAMESPACE" -o json >"$EVIDENCE/initial-service.json"
jq -e --slurpfile root "$EVIDENCE/initial-root.json" --slurpfile config "$EVIDENCE/initial-config.json" --slurpfile service "$EVIDENCE/initial-service.json" --arg stateUid "$STATE_UID" --arg rootUid "$ROOT_UID" --arg stateName "$STATE_NAME" '
  (.metadata.uid == $stateUid) and (.spec.root.uid == $rootUid) and (.spec.root.bundleDigest == $root[0].spec.program.digest) and
  ([.metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}]) and
  (.spec.state.inventory | length == 2) and
  ([.spec.state.inventory[] | select(.logicalId == "configmap/root-config" and .identity == {apiVersion:"v1",kind:"ConfigMap",namespace:$config[0].metadata.namespace,name:$config[0].metadata.name} and .uid == $config[0].metadata.uid and .resourceVersion == $config[0].metadata.resourceVersion and .bundleDigest == $root[0].spec.program.digest)] | length == 1) and
  ([.spec.state.inventory[] | select(.logicalId == "service/root-service" and .identity == {apiVersion:"v1",kind:"Service",namespace:$service[0].metadata.namespace,name:$service[0].metadata.name} and .uid == $service[0].metadata.uid and .resourceVersion == $service[0].metadata.resourceVersion and .bundleDigest == $root[0].spec.program.digest)] | length == 1) and
  ((.spec.state.pendingDependencies // []) | length == 0) and
  (.spec.state.applyTransaction == null and .spec.state.pendingPersist == null and .spec.state.pruneCandidate == null and .spec.state.generationAdvanceClosure == null and .spec.state.deletion == null) and
  ((.spec.state.automation.pending // []) | length == 0) and ((.spec.state.automation.completions // []) | length == 0) and
  ($root[0].metadata.uid == $rootUid and $root[0].status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and
  ($root[0].status.observedGeneration == $root[0].metadata.generation and $root[0].status.verifiedBundleDigest == $root[0].spec.program.digest) and
  ($root[0].metadata.finalizers | index("demo.oxideloop.dev/finalizer") != null) and
  ([$config[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}]) and
  ([$service[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])
' "$EVIDENCE/initial-state.json" >/dev/null || die 'bootstrap root, state, inventory, or ownership is not exact'

# P14-E durable preparation boundary. This deliberately uses already-owned
# children, so every prepared intent is UID/RV-pinned and the boundary cannot
# contain a POST. The failpoint crashes only after Prepare is durable and
# before dispatch authorization is persisted; restart must invalidate/replan.
ctx create configmap prepared-foreign-sentinel -n "$NAMESPACE" --from-literal=foreign=true
atomic_capture prepared-foreign-before.json ctx get configmap prepared-foreign-sentinel -n "$NAMESPACE" -o json || die 'failed to capture prepared-boundary foreign sentinel'
wait_json 'journal quiescence before PreparedNotIssued barrier' '(.spec.state.inventory | length == 2) and (.spec.state.applyTransaction == null) and (.spec.state.pendingPersist == null)' "oxideloopstate/$STATE_NAME"
atomic_capture prepared-not-issued-identities-before.json identity_snapshot || die 'failed to capture PreparedNotIssued identities before restart'
PREPARE_FIFO="$WORK/prepare-release.fifo"; mkfifo "$PREPARE_FIFO"; exec 8<>"$PREPARE_FIFO"; BARRIER_FD=8
PREPARE_POD=$(controller_pod)
PREPARE_POD_UID=$(ctx get pod -n "$NAMESPACE" "$PREPARE_POD" -o jsonpath='{.metadata.uid}')
PREPARE_RESTART=$(controller_status "$PREPARE_POD" | jq -er '.restartCount')
ctx exec -i -n "$NAMESPACE" "$PREPARE_POD" -c controller -- /usr/local/bin/oxideloop-e2e-failpoint-control --socket /run/oxideloop/failpoints/control.sock --point AfterPrepareBeforeDispatchAuthorization <&8 >"$EVIDENCE/prepared-not-issued-barrier.txt" 2>&1 &
BARRIER_PID=$!
elapsed=0
while ! grep -q '^ARMED$' "$EVIDENCE/prepared-not-issued-barrier.txt" 2>/dev/null; do [ "$elapsed" -lt "$TIMEOUT" ] || die 'PreparedNotIssued barrier client did not acknowledge ARMED'; sleep 1; elapsed=$((elapsed + 1)); done
ctx annotate webapplication root -n "$NAMESPACE" "oxideloop.dev/e2e-prepared-not-issued=$RUN_ID" --overwrite
elapsed=0
while ! grep -q '^REACHED$' "$EVIDENCE/prepared-not-issued-barrier.txt" 2>/dev/null; do [ "$elapsed" -lt "$TIMEOUT" ] || die 'PreparedNotIssued barrier did not reach REACHED'; sleep 1; elapsed=$((elapsed + 1)); done
wait_for_prepared_not_issued
jq -e '
  (.spec.state.pendingPersist.purpose == "Prepare" and .spec.state.pendingPersist.phase == "Prepare") and
  (.spec.state.applyTransaction.nextCommandLifecycle.state == "PreparedNotIssued") and
  (.spec.state.applyTransaction.nextCommandLifecycle.commandId | type == "string") and
  ([.spec.state.applyTransaction.preparedIntents[] | select(.operation == "Apply" and (.expectedUid | type == "string") and (.expectedResourceVersion | type == "string"))] | length == (.spec.state.applyTransaction.preparedIntents | length)) and
  ([.spec.state.applyTransaction.preparedIntents[] | select(.operation == "Apply" and (.expectedUid == null or .expectedResourceVersion == null))] | length == 0)
' "$EVIDENCE/prepared-not-issued-state.json" >/dev/null || die 'PreparedNotIssued journal was not exact and UID/RV-pinned without a POST intent'
atomic_capture prepared-not-issued-no-post.json jq -n --arg reason 'all prepared mutation intents were existing-object Apply operations with exact UID/resourceVersion pins; no POST was authorized or issued' '{schema:"oxideloop.dev/kind-e2e-prepared-not-issued/v1",lifecycle:"PreparedNotIssued",postAuthorized:false,postIssued:false,reason:$reason}' || die 'failed to capture no-POST PreparedNotIssued evidence'
atomic_capture prepared-foreign-midpoint.json ctx get configmap prepared-foreign-sentinel -n "$NAMESPACE" -o json || die 'failed to capture prepared-boundary foreign sentinel midpoint'
cmp -s "$EVIDENCE/prepared-foreign-before.json" "$EVIDENCE/prepared-foreign-midpoint.json" || die 'PreparedNotIssued midpoint touched or adopted the foreign sentinel'
printf 'RELEASE\n' >&8
elapsed=0; while ! grep -q '^RELEASED$' "$EVIDENCE/prepared-not-issued-barrier.txt" 2>/dev/null; do [ "$elapsed" -lt "$TIMEOUT" ] || die 'PreparedNotIssued barrier did not acknowledge RELEASE'; sleep 1; elapsed=$((elapsed + 1)); done
wait "$BARRIER_PID"; BARRIER_PID=''; exec 8>&-; BARRIER_FD=''
test "$(controller_pod)" = "$PREPARE_POD" || die 'PreparedNotIssued barrier unexpectedly replaced controller pod'
wait_for_exact_crash 'AfterPrepareBeforeDispatchAuthorization' "$PREPARE_POD" "$PREPARE_POD_UID" "$((PREPARE_RESTART + 1))"
atomic_capture controller-AfterPrepareBeforeDispatchAuthorization-previous.log ctx logs -n "$NAMESPACE" "$PREPARE_POD" -c controller --previous --tail=-1 || die 'missing PreparedNotIssued previous controller log'
ctx rollout status deployment/oxideloop-controller -n "$NAMESPACE" --timeout="${TIMEOUT}s"
wait_json 'PreparedNotIssued invalidation returned to empty journal' '(.spec.state.inventory | length == 2) and ((.spec.state.pendingDependencies // []) | length == 0) and (.spec.state.applyTransaction == null) and (.spec.state.pendingPersist == null) and (.spec.state.pruneCandidate == null) and (.spec.state.generationAdvanceClosure == null) and (.spec.state.deletion == null)' "oxideloopstate/$STATE_NAME"
atomic_capture prepared-not-issued-after-state.json ctx get oxideloopstate "$STATE_NAME" -n "$NAMESPACE" -o json || die 'failed to capture post-restart PreparedNotIssued state'
atomic_capture prepared-not-issued-identities-after.json identity_snapshot || die 'failed to capture PreparedNotIssued identities after restart'
cmp -s "$EVIDENCE/prepared-not-issued-identities-before.json" "$EVIDENCE/prepared-not-issued-identities-after.json" || die 'PreparedNotIssued recovery changed an existing root, state, ConfigMap, or Service identity'
atomic_capture prepared-foreign-after.json ctx get configmap prepared-foreign-sentinel -n "$NAMESPACE" -o json || die 'failed to capture prepared-boundary foreign sentinel after restart'
cmp -s "$EVIDENCE/prepared-foreign-before.json" "$EVIDENCE/prepared-foreign-after.json" || die 'PreparedNotIssued restart touched or adopted the foreign sentinel'

# Ownership transition: the existing managed children must be updated through
# controller SSA rather than recreated. The root annotation is deterministic
# per run and only retriggers reconciliation.
CONFIG_UID=$(ctx get configmap root-config -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')
CONFIG_RV=$(ctx get configmap root-config -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
SERVICE_UID=$(ctx get service root-service -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')
SERVICE_RV=$(ctx get service root-service -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
ctx annotate webapplication root -n "$NAMESPACE" "oxideloop.dev/e2e-ownership-transition=$RUN_ID" --overwrite
wait_for_same_uid_changed_rv 'ConfigMap ConditionalApply ownership transition' configmap/root-config "$CONFIG_UID" "$CONFIG_RV"
wait_for_same_uid_changed_rv 'Service ConditionalApply ownership transition' service/root-service "$SERVICE_UID" "$SERVICE_RV"
wait_json 'ownership transition state final convergence' '(.metadata.uid == $stateUid) and (.spec.state.inventory | length == 2) and ((.spec.state.pendingDependencies // []) | length == 0) and (.spec.state.applyTransaction == null and .spec.state.pendingPersist == null and .spec.state.pruneCandidate == null and .spec.state.generationAdvanceClosure == null and .spec.state.deletion == null)' "oxideloopstate/$STATE_NAME" --arg stateUid "$STATE_UID"
wait_json 'ownership transition root current status' '(.metadata.uid == $rootUid) and (.status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest)' webapplication/root --arg rootUid "$ROOT_UID" --arg stateName "$STATE_NAME" --arg stateUid "$STATE_UID"
test "$(ctx get configmap root-config -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')" = "$CONFIG_UID" || die 'ConfigMap ConditionalApply ownership transition changed its UID'
test "$(ctx get service root-service -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')" = "$SERVICE_UID" || die 'Service ConditionalApply ownership transition changed its UID'
OWNERSHIP_CONFIG_RV=$(ctx get configmap root-config -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
OWNERSHIP_SERVICE_RV=$(ctx get service root-service -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
capture_managed_field_ownership state "oxideloopstate/$STATE_NAME" main 'f:spec'
capture_managed_field_ownership config configmap/root-config main 'f:data'
capture_managed_field_ownership service service/root-service main 'f:spec'
capture_managed_field_ownership root-status webapplication/root status 'f:status'

# Restart after all ownership transitions while the Service remains present.
# Reconciliation is then retriggered once more to expose any ownership conflict
# before the independent Service-prune gate below.
atomic_capture_ownership ownership-restart-identities-before.json ownership_restart_identities || die 'failed to capture ownership restart identities before restart'
restart_controller 'ownership transition'
wait_json 'ownership restart Service remains present' '(.metadata.uid == $serviceUid)' service/root-service --arg serviceUid "$SERVICE_UID"
ctx annotate webapplication root -n "$NAMESPACE" "oxideloop.dev/e2e-ownership-restart=$RUN_ID" --overwrite
wait_for_same_uid_changed_rv 'ConfigMap ConditionalApply after ownership restart' configmap/root-config "$CONFIG_UID" "$OWNERSHIP_CONFIG_RV"
wait_for_same_uid_changed_rv 'Service ConditionalApply after ownership restart' service/root-service "$SERVICE_UID" "$OWNERSHIP_SERVICE_RV"
wait_json 'ownership restart root current status' '(.metadata.uid == $rootUid) and (.status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest)' webapplication/root --arg rootUid "$ROOT_UID" --arg stateName "$STATE_NAME" --arg stateUid "$STATE_UID"
atomic_capture_ownership ownership-restart-identities-after.json ownership_restart_identities || die 'failed to capture ownership restart identities after restart'
cmp -s "$EVIDENCE/ownership-restart-identities-before.json" "$EVIDENCE/ownership-restart-identities-after.json" || die 'ownership restart changed root, state, ConfigMap, or Service identities'

# Pruning a service must retain the same root/state identity.
ctx patch webapplication root -n "$NAMESPACE" --type merge -p '{"spec":{"expose":false}}'
wait_json 'service pruned' '.items | length == 0' service
test "$(root_uid)" = "$ROOT_UID" || die 'prune changed the root identity'
wait_json 'prune state final convergence' '(.spec.state.inventory | length == 1) and ((.spec.state.pendingDependencies // []) | length == 0) and (.spec.state.applyTransaction == null) and (.spec.state.pendingPersist == null) and (.spec.state.pruneCandidate == null) and (.spec.state.generationAdvanceClosure == null) and (.spec.state.deletion == null) and ((.spec.state.automation.pending // []) | length == 0)' "oxideloopstate/$STATE_NAME"
wait_json 'prune root current render status' '(.status.state.rootUid == .metadata.uid) and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest) and (.status.inputToken | type == "string") and (.status.cacheEpoch | type == "number")' webapplication/root
ctx get oxideloopstate -n "$NAMESPACE" -o json >"$EVIDENCE/post-prune-state.json"
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/post-prune-root.json"
ctx get configmap root-config -n "$NAMESPACE" -o json >"$EVIDENCE/post-prune-config.json"
jq -e --slurpfile root "$EVIDENCE/post-prune-root.json" --slurpfile config "$EVIDENCE/post-prune-config.json" --arg stateUid "$STATE_UID" --arg rootUid "$ROOT_UID" --arg stateName "$STATE_NAME" --arg configUid "$CONFIG_UID" '
  (.items | length == 1) and (.items[0].metadata.uid == $stateUid) and (.items[0].spec.root.uid == $rootUid) and
  ([.items[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}]) and
  (.items[0].spec.state.inventory | length == 1) and
  (.items[0].spec.state.inventory[0].logicalId == "configmap/root-config") and
  (.items[0].spec.state.inventory[0].identity == {apiVersion:"v1",kind:"ConfigMap",namespace:$config[0].metadata.namespace,name:$config[0].metadata.name}) and
  (.items[0].spec.state.inventory[0].uid == $configUid and .items[0].spec.state.inventory[0].resourceVersion == $config[0].metadata.resourceVersion and .items[0].spec.state.inventory[0].bundleDigest == $root[0].spec.program.digest) and
  ((.items[0].spec.state.pendingDependencies // []) | length == 0) and
  (.items[0].spec.state.applyTransaction == null and .items[0].spec.state.pendingPersist == null and .items[0].spec.state.pruneCandidate == null and .items[0].spec.state.generationAdvanceClosure == null and .items[0].spec.state.deletion == null) and
  ((.items[0].spec.state.automation.pending // []) | length == 0) and
  ($root[0].metadata.uid == $rootUid and $root[0].status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and
  ($root[0].status.observedGeneration == $root[0].metadata.generation and $root[0].status.verifiedBundleDigest == $root[0].spec.program.digest) and
  ([$config[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])
' "$EVIDENCE/post-prune-state.json" >/dev/null || die 'prune did not leave exact root, state, config inventory, and ownership'

# No source means no baseline and no action.
assert_count "$(ctx get automationruns -n "$NAMESPACE" -o json | jq '.items | length')" 0 'AutomationRuns before source creation'
assert_count "$(ctx get events.events.k8s.io -n "$NAMESPACE" -o json | jq '[.items[] | select(.reason == "InputChanged")] | length')" 0 'InputChanged Events before source creation'

# The first present observation establishes the durable baseline and produces
# no run or Event. Only later source transitions are automations.
ctx create configmap demo-input -n "$NAMESPACE" --from-literal=flavor=vanilla
wait_json 'dependency baseline rerender' '.data.flavor == "vanilla"' configmap/root-config
wait_json 'durable automation baseline' '(.spec.state.automation.baselines // [] | length == 1) and (.spec.state.automation.pending // [] | length == 0) and (.spec.state.automation.completions // [] | length == 0)' "oxideloopstate/$STATE_NAME"
INPUT_UID=$(ctx get configmap demo-input -n "$NAMESPACE" -o jsonpath='{.metadata.uid}')
INPUT_RV=$(ctx get configmap demo-input -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
assert_automation_transition 0 "$INPUT_UID" "$INPUT_RV" baseline
capture_transition baseline

# Every later observed source RV creates exactly one uniquely linked run and
# Event and one matching durable completion.
ctx patch configmap demo-input -n "$NAMESPACE" --type merge -p '{"data":{"flavor":"chocolate"}}'
wait_json 'rerendered dependency value' '.data.flavor == "chocolate"' configmap/root-config
wait_json 'first dependency automation' '.items | length == 1' automationrun
INPUT_RV=$(ctx get configmap demo-input -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
assert_automation_transition 1 "$INPUT_UID" "$INPUT_RV" chocolate
AUTOMATION_RUN_NAME=$(ctx get automationruns -n "$NAMESPACE" -o json | jq -er '[.items[] | select(.status.completion.state == "Succeeded") | .metadata.name] | if length == 1 then .[0] else error("expected exactly one completed AutomationRun for managed-fields evidence") end')
capture_managed_field_ownership automationrun-status "automationrun/$AUTOMATION_RUN_NAME" status 'f:status'
capture_transition chocolate

# The ambiguous event-materialization-before-status window is deliberately not
# a successful recovery path: a POST may have landed without a durable UID or
# admission witness, so it holds and requires operator intervention. Record
# that boundary while continuing with the declarative action flow.
atomic_capture ambiguous-post-window.json jq -n '{schema:"oxideloop.dev/kind-e2e-ambiguous-post-window/v1",status:"not-recoverable",successfulRecovery:false,requiresIntervention:true,reason:"event materialization may have accepted a POST before status persistence; without an admission witness, automatic recovery is unsafe"}' || die 'failed to capture ambiguous POST window statement'
ctx patch configmap demo-input -n "$NAMESPACE" --type merge -p '{"data":{"flavor":"strawberry"}}'
wait_json 'action after ambiguous POST window statement creates one durable action' '(.items | length == 2) and ([.items[].status.completion.state] | all(. == "Succeeded"))' automationrun
INPUT_RV=$(ctx get configmap demo-input -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
assert_automation_transition 2 "$INPUT_UID" "$INPUT_RV" strawberry
capture_transition strawberry
arm_crash AfterStatusBeforeAck
ctx patch configmap demo-input -n "$NAMESPACE" --type merge -p '{"data":{"flavor":"mint"}}'
await_crash AfterStatusBeforeAck
wait_json 'second crash recovery creates one durable action' '(.items | length == 3) and ([.items[].status.completion.state] | all(. == "Succeeded"))' automationrun
INPUT_RV=$(ctx get configmap demo-input -n "$NAMESPACE" -o jsonpath='{.metadata.resourceVersion}')
assert_automation_transition 3 "$INPUT_UID" "$INPUT_RV" mint
capture_transition mint

# An ordinary pod restart is a separate recovery proof.
RESTART_OLD_POD=$(controller_pod)
RESTART_OLD_POD_UID=$(ctx get pod -n "$NAMESPACE" "$RESTART_OLD_POD" -o jsonpath='{.metadata.uid}')
ctx get webapplications,oxideloopstates,configmaps,automationruns,events.events.k8s.io -n "$NAMESPACE" -o json | jq -S '
  {root:[.items[] | select(.kind == "WebApplication" and .metadata.name == "root") | {name:.metadata.name,uid:.metadata.uid}],
   state:[.items[] | select(.kind == "OxideLoopState") | {name:.metadata.name,uid:.metadata.uid}],
   config:[.items[] | select(.kind == "ConfigMap" and .metadata.name == "root-config") | {name:.metadata.name,uid:.metadata.uid}],
   runs:([.items[] | select(.kind == "AutomationRun") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name)),
   events:([.items[] | select(.kind == "Event" and .reason == "InputChanged") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name))}
' >"$EVIDENCE/ordinary-restart-identities-before.json"
ctx delete pod -n "$NAMESPACE" "$RESTART_OLD_POD" --wait=false
elapsed=0; RESTART_NEW_POD=''; RESTART_NEW_POD_UID=''
while [ -z "$RESTART_NEW_POD" ]; do
  restart_snapshot=$(ctx get pods -n "$NAMESPACE" -l app.kubernetes.io/component=controller -o json 2>/dev/null || true)
  restart_pair=$(printf '%s' "$restart_snapshot" | jq -r --arg oldUid "$RESTART_OLD_POD_UID" '[.items[] | select(.metadata.deletionTimestamp == null and .metadata.uid != $oldUid and ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length == 1)) | [.metadata.name,.metadata.uid] | @tsv] | if length == 1 then .[0] else empty end' 2>/dev/null || true)
  if [ -n "$restart_pair" ]; then
    RESTART_NEW_POD=$(printf '%s' "$restart_pair" | cut -f1)
    RESTART_NEW_POD_UID=$(printf '%s' "$restart_pair" | cut -f2)
    break
  fi
  [ "$elapsed" -lt "$TIMEOUT" ] || die 'timed out waiting for a new Ready controller pod after ordinary restart'
  sleep 2; elapsed=$((elapsed + 2))
done
[ "$RESTART_NEW_POD_UID" != "$RESTART_OLD_POD_UID" ] || die 'ordinary restart reused the controller pod UID'
ctx rollout status deployment/oxideloop-controller -n "$NAMESPACE" --timeout="${TIMEOUT}s"
ctx get webapplications,oxideloopstates,configmaps,automationruns,events.events.k8s.io -n "$NAMESPACE" -o json | jq -S '
  {root:[.items[] | select(.kind == "WebApplication" and .metadata.name == "root") | {name:.metadata.name,uid:.metadata.uid}],
   state:[.items[] | select(.kind == "OxideLoopState") | {name:.metadata.name,uid:.metadata.uid}],
   config:[.items[] | select(.kind == "ConfigMap" and .metadata.name == "root-config") | {name:.metadata.name,uid:.metadata.uid}],
   runs:([.items[] | select(.kind == "AutomationRun") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name)),
   events:([.items[] | select(.kind == "Event" and .reason == "InputChanged") | {name:.metadata.name,uid:.metadata.uid}] | sort_by(.name))}
' >"$EVIDENCE/ordinary-restart-identities-after.json"
cmp -s "$EVIDENCE/ordinary-restart-identities-before.json" "$EVIDENCE/ordinary-restart-identities-after.json" || die 'ordinary restart changed durable or managed object identities'
assert_count "$(ctx get automationruns -n "$NAMESPACE" -o json | jq '.items | length')" 3 'AutomationRuns after ordinary restart'
assert_automation_transition 3 "$INPUT_UID" "$INPUT_RV" post-ordinary-restart
capture_transition post-ordinary-restart

# Deletion barrier: the parent owns an RDWR descriptor, so opening the FIFO
# cannot deadlock if the client crashes before reading.  ARMED and REACHED are
# emitted by the private controller/client protocol; RELEASE is the only byte
# this parent can send after the midpoint snapshot has been captured.
FIFO="$WORK/release.fifo"; mkfifo "$FIFO"; exec 9<>"$FIFO"; BARRIER_FD=9
pod=$(controller_pod)
DELETE_POD_UID=$(ctx get pod -n "$NAMESPACE" "$pod" -o jsonpath='{.metadata.uid}')
DELETE_RESTART=$(controller_status "$pod" | jq -er '.restartCount')
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/deletion-before-root.json"
ctx get oxideloopstate "$STATE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE/deletion-before-state.json"
DELETE_ROOT_GENERATION=$(jq -er '.metadata.generation' "$EVIDENCE/deletion-before-root.json")
ctx exec -i -n "$NAMESPACE" "$pod" -c controller -- /usr/local/bin/oxideloop-e2e-failpoint-control --socket /run/oxideloop/failpoints/control.sock --point AfterStateFinalizerBeforeRootFinalizer <&9 >"$EVIDENCE/deletion-barrier.txt" 2>&1 &
BARRIER_PID=$!
elapsed=0
while ! grep -q '^ARMED$' "$EVIDENCE/deletion-barrier.txt" 2>/dev/null; do [ "$elapsed" -lt "$TIMEOUT" ] || die 'deletion barrier client did not acknowledge ARMED'; sleep 1; elapsed=$((elapsed + 1)); done
ctx delete webapplication root -n "$NAMESPACE" --wait=false
elapsed=0
while ! grep -q '^REACHED$' "$EVIDENCE/deletion-barrier.txt" 2>/dev/null; do [ "$elapsed" -lt "$TIMEOUT" ] || die 'deletion barrier did not reach REACHED'; sleep 1; elapsed=$((elapsed + 1)); done
sleep 2
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/deletion-midpoint-root.json"
ctx get oxideloopstates -n "$NAMESPACE" -o json >"$EVIDENCE/deletion-midpoint-state.json"
ctx get configmaps,services,automationruns -n "$NAMESPACE" -o json >"$EVIDENCE/deletion-midpoint-owned-children.json"
jq -e --arg uid "$ROOT_UID" '(.metadata.uid == $uid) and (.metadata.deletionTimestamp != null) and ([.metadata.finalizers[]?] | length > 0)' "$EVIDENCE/deletion-midpoint-root.json" >/dev/null || die 'deletion midpoint lacks original deleting root/finalizer'
assert_count "$(jq '.items | length' "$EVIDENCE/deletion-midpoint-state.json")" 0 'deletion midpoint state absence'
jq -e --arg rootUid "$ROOT_UID" '[.items[] | select(any(.metadata.ownerReferences[]?; .uid == $rootUid))] | length == 0' "$EVIDENCE/deletion-midpoint-owned-children.json" >/dev/null || die 'deletion midpoint still has a ConfigMap, Service, or AutomationRun owned by the original root'
jq -e --arg rootUid "$ROOT_UID" --arg stateUid "$STATE_UID" --arg stateName "$STATE_NAME" --argjson generation "$DELETE_ROOT_GENERATION" '
  .status.deletionCleanupReceipt as $receipt |
  (.metadata.uid == $rootUid) and (.metadata.deletionTimestamp != null) and
  (.metadata.finalizers | index("demo.oxideloop.dev/finalizer") != null) and
  ($receipt.rootUid == $rootUid and $receipt.stateUid == $stateUid and $receipt.stateName == $stateName and $receipt.deletionStartGeneration == $generation) and
  ($receipt.deletionId | test("^[0-9a-f]{64}$")) and ($receipt.cleanupToken | test("^[0-9a-f]{64}$"))
' "$EVIDENCE/deletion-midpoint-root.json" >/dev/null || die 'deletion midpoint receipt is not exact for the original root and state'
printf 'RELEASE\n' >&9
elapsed=0; while ! grep -q '^RELEASED$' "$EVIDENCE/deletion-barrier.txt" 2>/dev/null; do [ "$elapsed" -lt "$TIMEOUT" ] || die 'deletion barrier did not acknowledge RELEASE'; sleep 1; elapsed=$((elapsed + 1)); done
wait "$BARRIER_PID"; BARRIER_PID=''; exec 9>&-; BARRIER_FD=''
test "$(controller_pod)" = "$pod" || die 'deletion barrier unexpectedly replaced controller pod'
wait_for_exact_crash 'deletion barrier' "$pod" "$DELETE_POD_UID" "$((DELETE_RESTART + 1))"
atomic_capture controller-deletion-previous.log ctx logs -n "$NAMESPACE" "$pod" -c controller --previous --tail=-1 || die 'missing deletion previous log'
wait_json 'original root absence after RELEASE' '.items | length == 0' webapplications
if ctx get webapplication root -n "$NAMESPACE" >/dev/null 2>&1; then die 'original root still exists after deletion'; fi
elapsed=0
while :; do
  deletion_status=$(controller_status "$pod" 2>/dev/null || true)
  deletion_uid=$(ctx get pod -n "$NAMESPACE" "$pod" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
  [ "$deletion_uid" = "$DELETE_POD_UID" ] || die 'root-only recovery replaced the exact controller pod'
  deletion_restarts=$(printf '%s' "$deletion_status" | jq -r '.restartCount // empty')
  deletion_exit=$(printf '%s' "$deletion_status" | jq -r '.lastState.terminated.exitCode // empty')
  [ "$deletion_restarts" -le "$((DELETE_RESTART + 2))" ] 2>/dev/null || die 'root-only deletion recovery restarted more than once'
  if [ "$deletion_restarts" = "$((DELETE_RESTART + 2))" ] && [ "$deletion_exit" = 0 ]; then break; fi
  [ "$elapsed" -lt "$TIMEOUT" ] || die 'timed out waiting for one successful root-only recovery process exit'
  sleep 1; elapsed=$((elapsed + 1))
done
atomic_capture controller-root-only-recovery-previous.log ctx logs -n "$NAMESPACE" "$pod" -c controller --previous --tail=-1 || die 'missing root-only recovery log'
grep -F "oxideloop: entering receipt-bounded root-only deletion recovery for $NAMESPACE/root uid=$ROOT_UID" "$EVIDENCE/controller-root-only-recovery-previous.log" >/dev/null || die 'post-crash process did not enter receipt-bounded root-only recovery'
ctx get all -n "$NAMESPACE" -o yaml >"$EVIDENCE/post-delete-inventory.yaml"
ctx get webapplications,oxideloopstates,automationruns -n "$NAMESPACE" -o json >"$EVIDENCE/post-delete-controller-inventory.json"
if ctx get configmap root-config -n "$NAMESPACE" >/dev/null 2>&1; then die 'managed ConfigMap remains after deletion'; fi
assert_count "$(ctx get oxideloopstates -n "$NAMESPACE" -o json | jq '.items | length')" 0 'post-delete durable inventory'
assert_count "$(ctx get automationruns -n "$NAMESPACE" -o json | jq '.items | length')" 0 'post-delete AutomationRun inventory'
assert_count "$(ctx get service -n "$NAMESPACE" -o json | jq '.items | length')" 0 'post-delete Service inventory'
wait_json 'post-delete InputChanged Event garbage collection' '[.items[] | select(.reason == "InputChanged")] | length == 0' events.events.k8s.io
ctx get configmaps,services,automationruns,events.events.k8s.io -n "$NAMESPACE" -o json >"$EVIDENCE/post-delete-owned-objects.json"
jq -e --arg rootUid "$ROOT_UID" '
  ([.items[] | select(any(.metadata.ownerReferences[]?; .uid == $rootUid))] | length == 0) and
  ([.items[] | select(.kind == "Event" and .reason == "InputChanged")] | length == 0)
' "$EVIDENCE/post-delete-owned-objects.json" >/dev/null || die 'post-delete namespace retains an original-root child or action Event'
atomic_capture prepared-foreign-after-final-root-deletion.json ctx get configmap prepared-foreign-sentinel -n "$NAMESPACE" -o json || die 'final root deletion removed the foreign sentinel'
cmp -s "$EVIDENCE/prepared-foreign-before.json" "$EVIDENCE/prepared-foreign-after-final-root-deletion.json" || die 'final root deletion mutated the foreign sentinel'
jq -e '.metadata.ownerReferences == null or (.metadata.ownerReferences | length == 0)' "$EVIDENCE/prepared-foreign-after-final-root-deletion.json" >/dev/null || die 'final root deletion adopted the foreign sentinel'

# Only after the original UID is absent may the same name be replaced.
write_root; ctx apply -f "$WORK/root.yaml"
wait_json 'replacement current render status and durable link' '(.status.state.rootUid == .metadata.uid) and (.status.state.name | type == "string") and (.status.state.uid | type == "string") and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest) and (.status.inputToken | type == "string") and (.status.cacheEpoch | type == "number")' webapplication/root
REPLACEMENT_UID=$(root_uid)
[ "$REPLACEMENT_UID" != "$ROOT_UID" ] || die 'replacement reused original UID'
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/replacement-root.json"
REPLACEMENT_STATE_NAME=$(jq -er '.status.state.name' "$EVIDENCE/replacement-root.json")
REPLACEMENT_STATE_UID=$(jq -er '.status.state.uid' "$EVIDENCE/replacement-root.json")
wait_json 'replacement state and baseline final convergence' '(.spec.state.inventory | length == 2) and ((.spec.state.pendingDependencies // []) | length == 0) and (.spec.state.applyTransaction == null) and (.spec.state.pendingPersist == null) and (.spec.state.pruneCandidate == null) and (.spec.state.generationAdvanceClosure == null) and (.spec.state.deletion == null) and ((.spec.state.automation.baselines // []) | length == 1) and ((.spec.state.automation.pending // []) | length == 0) and ((.spec.state.automation.completions // []) | length == 0)' "oxideloopstate/$REPLACEMENT_STATE_NAME"
wait_json 'replacement root current render status' '(.status.state.rootUid == .metadata.uid) and (.status.observedGeneration == .metadata.generation) and (.status.verifiedBundleDigest == .spec.program.digest) and (.status.inputToken | type == "string") and (.status.cacheEpoch | type == "number")' webapplication/root
ctx get webapplication root -n "$NAMESPACE" -o json >"$EVIDENCE/replacement-root.json"
ctx get oxideloopstate -n "$NAMESPACE" -o json >"$EVIDENCE/replacement-inventory.json"
ctx get configmap root-config -n "$NAMESPACE" -o json >"$EVIDENCE/replacement-config.json"
ctx get service root-service -n "$NAMESPACE" -o json >"$EVIDENCE/replacement-service.json"
jq -e --slurpfile root "$EVIDENCE/replacement-root.json" --slurpfile config "$EVIDENCE/replacement-config.json" --slurpfile service "$EVIDENCE/replacement-service.json" --arg namespace "$NAMESPACE" --arg oldRootUid "$ROOT_UID" --arg oldStateUid "$STATE_UID" --arg rootUid "$REPLACEMENT_UID" --arg stateName "$REPLACEMENT_STATE_NAME" --arg stateUid "$REPLACEMENT_STATE_UID" '
  (.items | length == 1) and (.items[0].metadata.name == $stateName and .items[0].metadata.uid == $stateUid and .items[0].metadata.uid != $oldStateUid) and
  (.items[0].spec.root.uid == $rootUid and .items[0].spec.root.uid != $oldRootUid and .items[0].spec.root.bundleDigest == $root[0].spec.program.digest) and
  ([.items[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}]) and
  (.items[0].spec.state.inventory | length == 2) and
  ([.items[0].spec.state.inventory[] | select(.logicalId == "configmap/root-config" and .identity == {apiVersion:"v1",kind:"ConfigMap",namespace:$namespace,name:"root-config"} and .uid == $config[0].metadata.uid and .resourceVersion == $config[0].metadata.resourceVersion and .bundleDigest == $root[0].spec.program.digest)] | length == 1) and
  ([.items[0].spec.state.inventory[] | select(.logicalId == "service/root-service" and .identity == {apiVersion:"v1",kind:"Service",namespace:$namespace,name:"root-service"} and .uid == $service[0].metadata.uid and .resourceVersion == $service[0].metadata.resourceVersion and .bundleDigest == $root[0].spec.program.digest)] | length == 1) and
  ((.items[0].spec.state.pendingDependencies // []) | length == 0) and
  (.items[0].spec.state.applyTransaction == null and .items[0].spec.state.pendingPersist == null and .items[0].spec.state.pruneCandidate == null and .items[0].spec.state.generationAdvanceClosure == null and .items[0].spec.state.deletion == null) and
  ((.items[0].spec.state.automation.baselines // []) | length == 1) and ((.items[0].spec.state.automation.pending // []) | length == 0) and ((.items[0].spec.state.automation.completions // []) | length == 0) and
  ($root[0].metadata.uid == $rootUid and $root[0].status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and
  ($root[0].status.observedGeneration == $root[0].metadata.generation and $root[0].status.verifiedBundleDigest == $root[0].spec.program.digest) and
  ($config[0].data.flavor == "mint") and
  ([$config[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}]) and
  ([$service[0].metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$rootUid,controller:true,blockOwnerDeletion:false}])
' "$EVIDENCE/replacement-inventory.json" >/dev/null || die 'replacement did not prove a new root/state/owned inventory without original identities'
assert_count "$(ctx get automationruns -n "$NAMESPACE" -o json | jq '.items | length')" 0 'replacement fresh baseline AutomationRun inventory'
assert_count "$(ctx get events.events.k8s.io -n "$NAMESPACE" -o json | jq '[.items[] | select(.reason == "InputChanged")] | length')" 0 'replacement fresh baseline Event inventory'
note 'matrix passed'
