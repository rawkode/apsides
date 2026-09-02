#!/usr/bin/env bash
# Presenter-friendly happy-path demo. The exhaustive fault matrix lives in
# run-kind-e2e.sh; this script is intentionally short and interactive.
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$ROOT"

RUN_ID=${RUN_ID:-"$(date -u +%Y%m%dt%H%M%Sz)-$$"}
CLUSTER_NAME=${KIND_CLUSTER_NAME:-"oxideloop-demo-$RUN_ID"}
NAMESPACE=${DEMO_NAMESPACE:-oxideloop-demo}
TIMEOUT=${DEMO_TIMEOUT_SECONDS:-180}
KEEP_CLUSTER=${KEEP_CLUSTER:-0}
PAUSE=${DEMO_PAUSE:-0}
SHOW_SOURCE=${SHOW_SOURCE:-1}
WORK="$ROOT/work/demo/$RUN_ID"
CONTEXT="kind-$CLUSTER_NAME"
IMAGE="oxideloop-demo:local-$RUN_ID"
CLUSTER_CREATED=false

say() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
fail() { printf '\nDemo failed: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "required command is unavailable: $1"; }
pause() {
  [ "$PAUSE" = 1 ] || return 0
  printf 'Press Return to continue... '
  read -r _
}
ctx() { kubectl --request-timeout=20s --context "$CONTEXT" -n "$NAMESPACE" "$@"; }
wait_json() {
  description=$1; resource=$2; filter=$3
  elapsed=0
  until ctx get "$resource" -o json 2>/dev/null | jq -e "$filter" >/dev/null; do
    [ "$elapsed" -lt "$TIMEOUT" ] || fail "timed out waiting for $description"
    sleep 2
    elapsed=$((elapsed + 2))
  done
}
wait_json_args() {
  description=$1; resource=$2; filter=$3; shift 3
  elapsed=0
  until ctx get "$resource" -o json 2>/dev/null | jq -e "$@" "$filter" >/dev/null; do
    [ "$elapsed" -lt "$TIMEOUT" ] || fail "timed out waiting for $description"
    sleep 2; elapsed=$((elapsed + 2))
  done
}
assert_count() { actual=$1; expected=$2; label=$3; [ "$actual" = "$expected" ] || fail "$label: expected $expected, got $actual"; }
cleanup() {
  status=$?
  if [ "$status" -ne 0 ] && [ "$CLUSTER_CREATED" = true ]; then
    mkdir -p "$WORK"
    ctx logs deployment/oxideloop-controller --all-containers --tail=200 >"$WORK/controller.log" 2>&1 || true
    printf 'Controller logs: %s\n' "$WORK/controller.log" >&2
  fi
  if [ "$CLUSTER_CREATED" = true ] && [ "$KEEP_CLUSTER" != 1 ]; then
    say "Deleting demo cluster $CLUSTER_NAME"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  elif [ "$CLUSTER_CREATED" = true ]; then
    printf '\nCluster kept. Inspect it with:\n  kubectl --context %s -n %s get all,webapplications,oxideloopstates,automationruns\n' "$CONTEXT" "$NAMESPACE"
  fi
  exit "$status"
}
trap cleanup EXIT

for command in docker kind kubectl jq pnpm rg shasum; do need "$command"; done
docker info >/dev/null 2>&1 || fail 'Docker or OrbStack is not running'
case "$RUN_ID" in *[!a-z0-9._-]*|'') fail 'RUN_ID must contain only lowercase letters, digits, dot, underscore, or hyphen';; esac
[ "$TIMEOUT" -gt 0 ] 2>/dev/null || fail 'DEMO_TIMEOUT_SECONDS must be positive'
if kind get clusters 2>/dev/null | awk -v name="$CLUSTER_NAME" '$0 == name { found=1 } END { exit !found }'; then
  fail "cluster already exists: $CLUSTER_NAME"
fi
mkdir -p "$WORK"
export KIND_EXPERIMENTAL_PROVIDER=${KIND_EXPERIMENTAL_PROVIDER:-docker}

say 'User-authored controller'
printf '%s\n' 'packages/example-bundle/src/controller.tsx'
if [ "$SHOW_SOURCE" = 1 ]; then sed -n '1,180p' packages/example-bundle/src/controller.tsx; fi
pause

say "Creating fresh kind cluster $CLUSTER_NAME"
kind create cluster \
  --name "$CLUSTER_NAME" \
  --config deploy/e2e/kind-config.yaml \
  --wait "${TIMEOUT}s"
CLUSTER_CREATED=true
ctx create namespace "$NAMESPACE"
ctx apply -f deploy/crds/oxideloop-crds.yaml
kubectl --context "$CONTEXT" wait \
  --for=condition=Established \
  crd/webapplications.demo.oxideloop.dev \
  --timeout="${TIMEOUT}s"

say 'Compiling the TSX controller into an immutable bundle'
pnpm install --frozen-lockfile
pnpm bundle:e2e -- \
  --out "$WORK/bundle.archive" \
  --metadata "$WORK/bundle-metadata.json"
jq -e '.buildBundle.authoredSource == {location:"packages/example-bundle/src",controller:"packages/example-bundle/src/controller.tsx"}' "$WORK/bundle-metadata.json" >/dev/null || fail 'bundle metadata source identity is not exact'
BUNDLE_DIGEST=$(jq -er '.buildBundle.bundleDigest' "$WORK/bundle-metadata.json")
BUNDLE_SHA=$(jq -er '.buildBundle.archive.sha256' "$WORK/bundle-metadata.json")
test "$(shasum -a 256 "$WORK/bundle.archive" | awk '{print $1}')" = "$BUNDLE_SHA" || fail 'bundle archive SHA-256 does not match metadata'
printf 'Bundle digest: %s\n' "$BUNDLE_DIGEST"
ctx create configmap oxideloop-demo-bundle \
  --from-file="bundle.archive=$WORK/bundle.archive" \
  --dry-run=client -o yaml | ctx apply -f -
pause

say 'Building and installing the Rust controller plus isolated renderer'
docker build --provenance=false --target production -t "$IMAGE" .
kind load docker-image --name "$CLUSTER_NAME" "$IMAGE"
kubectl kustomize deploy/controller |
  sed "s#ghcr.io/oxideloop/oxideloop:production#$IMAGE#g" |
  ctx apply -f -
ctx rollout status deployment/oxideloop-controller --timeout="${TIMEOUT}s"

say 'Applying one WebApplication custom resource'
ctx apply -f - <<EOF
apiVersion: demo.oxideloop.dev/v1alpha1
kind: WebApplication
metadata:
  name: root
spec:
  image: $IMAGE
  replicas: 0
  expose: true
  program:
    configMapName: oxideloop-demo-bundle
    key: bundle.archive
    digest: $BUNDLE_DIGEST
EOF

wait_json 'verified root status' webapplication/root \
  '(.status.state.name | type == "string") and (.status.verifiedBundleDigest == .spec.program.digest)'
wait_json 'rendered ConfigMap' configmap/root-config '.data.message == "Hello from OxideLoop"'
wait_json 'rendered Deployment' deployment/root '.metadata.uid | type == "string"'
wait_json 'rendered Service' service/root-service '.metadata.uid | type == "string"'
ROOT_UID=$(ctx get webapplication/root -o jsonpath='{.metadata.uid}')
ROOT_JSON="$WORK/root.json"
STATE_JSON="$WORK/state.json"
ctx get webapplication/root -o json >"$ROOT_JSON"
STATE_NAME=$(jq -er '.status.state.name' "$ROOT_JSON")
ctx get oxideloopstate "$STATE_NAME" -o json >"$STATE_JSON"
jq -e --arg rootUid "$ROOT_UID" --arg stateName "$STATE_NAME" --arg stateUid "$(jq -er '.metadata.uid' "$STATE_JSON")" \
  '(.metadata.uid == $rootUid) and (.status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}) and (.metadata.finalizers | index("demo.oxideloop.dev/finalizer") != null)' "$ROOT_JSON" >/dev/null \
  || fail 'root status/state link or root finalizer is not exact'
jq -e '.metadata.finalizers | index("runtime.oxideloop.dev/state-finalizer") != null' "$STATE_JSON" >/dev/null \
  || fail 'State finalizer is missing'
if ctx logs deployment/oxideloop-controller --all-containers --tail=200 2>/dev/null | rg -q 'unlinked root'; then fail 'controller reported an unlinked-root hold during initial convergence'; fi
for resource in configmap/root-config deployment/root service/root-service; do
  ctx get "$resource" -o json | jq -e --arg uid "$ROOT_UID" \
    '[.metadata.ownerReferences[]?] == [{apiVersion:"demo.oxideloop.dev/v1alpha1",kind:"WebApplication",name:"root",uid:$uid,controller:true,blockOwnerDeletion:false}]' >/dev/null || fail "$resource owner reference is not exact"
done

say 'The TSX rendered three owned Kubernetes resources'
ctx get webapplication/root configmap/root-config deployment/root service/root-service
ctx get configmap/root-config -o json | jq '{message:.data.message,owner:.metadata.ownerReferences[0]}'
pause

say 'Establishing the useResource() baseline'
assert_count "$(ctx get automationruns -o json | jq '.items | length')" 0 'baseline AutomationRuns'
assert_count "$(ctx get events.events.k8s.io -o json | jq '[.items[] | select(.reason == "ConfigurationChanged" and .action == "ConfigurationChanged")] | length')" 0 'baseline ConfigurationChanged Events'
ctx create configmap demo-input --from-literal=message=vanilla
wait_json 'dependency-driven vanilla render' configmap/root-config '.data.message == "vanilla"'
INPUT_UID=$(ctx get configmap/demo-input -o jsonpath='{.metadata.uid}')
BASELINE_RV=$(ctx get configmap/demo-input -o jsonpath='{.metadata.resourceVersion}')
wait_json 'durable automation baseline' "oxideloopstate/$STATE_NAME" \
  '((.spec.state.automation.baselines // []) | length == 1) and ((.spec.state.automation.pending // []) | length == 0) and ((.spec.state.automation.completions // []) | length == 0)'
assert_count "$(ctx get automationruns -o json | jq '.items | length')" 0 'post-baseline AutomationRuns'
assert_count "$(ctx get events.events.k8s.io -o json | jq '[.items[] | select(.reason == "ConfigurationChanged" and .action == "ConfigurationChanged")] | length')" 0 'post-baseline ConfigurationChanged Events'
printf 'Baseline source UID/RV: %s / %s\n' "$INPUT_UID" "$BASELINE_RV"
printf 'root-config message: '
ctx get configmap/root-config -o jsonpath='{.data.message}'
printf '\nAutomation runs at baseline: '
ctx get automationruns -o json | jq '.items | length'
pause

say 'Changing demo-input: the hook rerenders and the automation emits an Event'
ctx patch configmap/demo-input --type merge -p '{"data":{"message":"chocolate"}}' >/dev/null
wait_json 'dependency-driven chocolate render' configmap/root-config '.data.message == "chocolate"'
CURRENT_RV=$(ctx get configmap/demo-input -o jsonpath='{.metadata.resourceVersion}')
wait_json_args 'exactly one successful correlated AutomationRun' automationruns \
  '(.items | length == 1) and ([.items[] | select(.status.completion.state == "Succeeded" and .spec.sourceUid == $uid and .spec.sourceResourceVersion == $rv)] | length == 1)' \
  --arg uid "$INPUT_UID" --arg rv "$CURRENT_RV"
RUN_JSON=$(ctx get automationruns -o json)
RUN_NAME=$(printf '%s' "$RUN_JSON" | jq -er '.items[0].metadata.name')
RUN_UID=$(printf '%s' "$RUN_JSON" | jq -er '.items[0].metadata.uid')
wait_json_args 'exactly one correlated ConfigurationChanged Event' events.events.k8s.io \
  '(.items | map(select(.reason == "ConfigurationChanged" and .action == "ConfigurationChanged")) | length == 1) and ([.items[] | select(.reason == "ConfigurationChanged" and .action == "ConfigurationChanged" and .note == "chocolate" and .regarding.uid == $uid and .regarding.resourceVersion == $rv and .metadata.ownerReferences[0].name == $runName and .metadata.ownerReferences[0].uid == $runUid)] | length == 1)' \
  --arg uid "$INPUT_UID" --arg rv "$CURRENT_RV" --arg runName "$RUN_NAME" --arg runUid "$RUN_UID"
printf 'Current source UID/RV: %s / %s\n' "$INPUT_UID" "$CURRENT_RV"

say 'Live result'
ctx get configmap/root-config -o json >"$WORK/final-configmap.json"
ctx get automationruns -o json >"$WORK/final-automationruns.json"
ctx get events.events.k8s.io -o json >"$WORK/final-events.json"
jq '{message:.data.message,resourceVersion:.metadata.resourceVersion}' "$WORK/final-configmap.json"
jq -r '.items[] | [.metadata.name,.spec.sourceResourceVersion,.status.completion.state] | @tsv' "$WORK/final-automationruns.json"
jq -r '.items[] | select(.reason == "ConfigurationChanged") | [.metadata.name,.reason,.note] | @tsv' "$WORK/final-events.json"

say 'Demo complete'
printf 'Artifacts from the compiler are in %s\n' "$WORK"
pause
