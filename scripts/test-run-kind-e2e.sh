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

# Extract the runner's exact Docker-identity parser so these fixtures exercise
# the same implementation without constructing a cluster. Docker 29 reports a
# manifest digest as `.Id`; Kubernetes reports the config digest instead.
sed -n '/^image_identity_from_inspect() {/,/^}/p' "$ROOT/scripts/run-kind-e2e.sh" >"$TEMP/image-identity.sh"
. "$TEMP/image-identity.sh"
manifest_digest="sha256:1111111111111111111111111111111111111111111111111111111111111111"
config_digest="sha256:2222222222222222222222222222222222222222222222222222222222222222"
cat >"$TEMP/docker29-inspect.json" <<EOF
[{"Id":"$manifest_digest","RepoTags":["oxideloop-e2e:production-test"],"Os":"linux","Architecture":"amd64","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"$manifest_digest","size":1234,"annotations":{"config.digest":"$config_digest"}}}]
EOF
image_identity_from_inspect "oxideloop-e2e:production-test" "$TEMP/docker29-inspect.json" >"$TEMP/docker29-identity.json"
jq -e --arg manifest "$manifest_digest" --arg config "$config_digest" '.manifestDigest == $manifest and .configDigest == $config and .configDigestSource == "Descriptor.annotations.config.digest" and .platform == {os:"linux",architecture:"amd64",variant:null}' "$TEMP/docker29-identity.json" >/dev/null
jq '.[] .Descriptor.mediaType = "application/vnd.docker.distribution.manifest.v2+json"' "$TEMP/docker29-inspect.json" >"$TEMP/docker-v2-inspect.json"
image_identity_from_inspect "oxideloop-e2e:production-test" "$TEMP/docker-v2-inspect.json" >/dev/null
cat >"$TEMP/mismatched-platform-inspect.json" <<EOF
[{"Id":"$manifest_digest","RepoTags":["oxideloop-e2e:mismatched-platform-test"],"Os":"linux","Architecture":"amd64","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"$manifest_digest","size":1234,"platform":{"os":"linux","architecture":"arm64"},"annotations":{"config.digest":"$config_digest"}}}]
EOF
if image_identity_from_inspect "oxideloop-e2e:mismatched-platform-test" "$TEMP/mismatched-platform-inspect.json" >/dev/null 2>&1; then
  echo 'mismatched Docker descriptor platform was accepted' >&2
  exit 1
fi
cat >"$TEMP/disagreeing-inspect.json" <<EOF
[{"Id":"$config_digest","RepoTags":["oxideloop-e2e:disagreeing-test"],"Os":"linux","Architecture":"amd64","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"$manifest_digest","size":1234,"platform":{"os":"linux","architecture":"amd64"},"annotations":{"config.digest":"$config_digest"}}}]
EOF
if image_identity_from_inspect "oxideloop-e2e:disagreeing-test" "$TEMP/disagreeing-inspect.json" >/dev/null 2>&1; then
  echo 'Docker inspect Id/manifest disagreement was accepted' >&2
  exit 1
fi

# Older supported Docker inspect forms can omit Descriptor. Its `.Id` is then
# the only exact config identity; no manifest value is invented.
cat >"$TEMP/legacy-inspect.json" <<EOF
[{"Id":"$config_digest","RepoTags":["oxideloop-e2e:legacy-test"],"Os":"linux","Architecture":"amd64"}]
EOF
image_identity_from_inspect "oxideloop-e2e:legacy-test" "$TEMP/legacy-inspect.json" >"$TEMP/legacy-identity.json"
jq -e --arg config "$config_digest" '.manifestDigest == null and .configDigest == $config and .configDigestSource == "Id (legacy descriptor unavailable)"' "$TEMP/legacy-identity.json" >/dev/null

# If modern inspect exposes only an indistinguishable manifest `.Id`, it is
# unsafe to call that a config digest. The parser must fail rather than allow
# the former manifest-versus-config comparison to pass.
cat >"$TEMP/ambiguous-inspect.json" <<EOF
[{"Id":"$manifest_digest","RepoTags":["oxideloop-e2e:ambiguous-test"],"Os":"linux","Architecture":"amd64","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"$manifest_digest","size":1234,"platform":{"os":"linux","architecture":"amd64"},"annotations":{}}}]
EOF
if image_identity_from_inspect "oxideloop-e2e:ambiguous-test" "$TEMP/ambiguous-inspect.json" >/dev/null 2>&1; then
  echo 'ambiguous Docker manifest identity was accepted as a config digest' >&2
  exit 1
fi

# Descriptor values must identify a concrete single-platform manifest. Indexes,
# missing/unknown media types, non-positive sizes, and malformed variants are
# deliberately rejected before any tag reaches kind.
for inspect_case in \
  '[{"mediaType":"application/vnd.oci.image.index.v1+json","size":1234}]' \
  '[{"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json","size":1234}]' \
  '[{"mediaType":"application/example","size":1234}]' \
  '[{"mediaType":null,"size":1234}]' \
  '[{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":null}]' \
  '[{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":0}]' \
  '[{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":1.5}]' \
  '[{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":1,"variant":1}]' \
  '[{"mediaType":"application/vnd.oci.image.manifest.v1+json","size":1,"variant":""}]'; do
  jq --arg manifest "$manifest_digest" --arg config "$config_digest" --argjson replacement "$inspect_case" '
    .[0].Variant = ($replacement[0].variant // .[0].Variant) |
    .[0].Descriptor.mediaType = $replacement[0].mediaType |
    .[0].Descriptor.size = $replacement[0].size
  ' "$TEMP/docker29-inspect.json" >"$TEMP/rejected-inspect.json"
  if image_identity_from_inspect "oxideloop-e2e:production-test" "$TEMP/rejected-inspect.json" >/dev/null 2>&1; then
    echo 'invalid Docker descriptor identity was accepted' >&2
    exit 1
  fi
done
jq '.[] .Descriptor.platform = {os:"linux",architecture:"amd64",variant:1}' "$TEMP/docker29-inspect.json" >"$TEMP/numeric-descriptor-variant.json"
if image_identity_from_inspect "oxideloop-e2e:production-test" "$TEMP/numeric-descriptor-variant.json" >/dev/null 2>&1; then
  echo 'numeric descriptor platform variant was accepted' >&2
  exit 1
fi
jq '.[] .Descriptor.platform = {os:"linux",architecture:"amd64",variant:""}' "$TEMP/docker29-inspect.json" >"$TEMP/empty-descriptor-variant.json"
if image_identity_from_inspect "oxideloop-e2e:production-test" "$TEMP/empty-descriptor-variant.json" >/dev/null 2>&1; then
  echo 'empty descriptor platform variant was accepted' >&2
  exit 1
fi
assert_inspect_rejected() {
  if image_identity_from_inspect "oxideloop-e2e:production-test" "$1" >/dev/null 2>&1; then
    echo "invalid Docker optional field was accepted: $1" >&2
    exit 1
  fi
}
# jq's `//` treats false like absence. Each false fixture is direct (rather
# than produced through `//`) to prove optional fields only accept missing/null
# where that is expressly supported.
jq '.[0].Descriptor = false' "$TEMP/docker29-inspect.json" >"$TEMP/descriptor-false.json"; assert_inspect_rejected "$TEMP/descriptor-false.json"
jq '.[0].Variant = false' "$TEMP/docker29-inspect.json" >"$TEMP/top-variant-false.json"; assert_inspect_rejected "$TEMP/top-variant-false.json"
jq '.[0].Descriptor.platform = false' "$TEMP/docker29-inspect.json" >"$TEMP/platform-false.json"; assert_inspect_rejected "$TEMP/platform-false.json"
jq '.[0].Descriptor.platform = {os:"linux",architecture:"amd64",variant:false}' "$TEMP/docker29-inspect.json" >"$TEMP/platform-variant-false.json"; assert_inspect_rejected "$TEMP/platform-variant-false.json"
jq '.[0].Descriptor.annotations = false' "$TEMP/docker29-inspect.json" >"$TEMP/annotations-false.json"; assert_inspect_rejected "$TEMP/annotations-false.json"
jq '.[0].Descriptor.annotations = []' "$TEMP/docker29-inspect.json" >"$TEMP/annotations-array.json"; assert_inspect_rejected "$TEMP/annotations-array.json"
jq '.[0].Descriptor.annotations["config.digest"] = false' "$TEMP/docker29-inspect.json" >"$TEMP/config-digest-false.json"; assert_inspect_rejected "$TEMP/config-digest-false.json"
jq '.[0].Descriptor.mediaType = false' "$TEMP/docker29-inspect.json" >"$TEMP/media-type-false.json"; assert_inspect_rejected "$TEMP/media-type-false.json"
jq '.[0].Descriptor.size = false' "$TEMP/docker29-inspect.json" >"$TEMP/size-false.json"; assert_inspect_rejected "$TEMP/size-false.json"
jq '.[0].Descriptor.digest = false' "$TEMP/docker29-inspect.json" >"$TEMP/manifest-digest-false.json"; assert_inspect_rejected "$TEMP/manifest-digest-false.json"
jq '.[0].RepoTags = false' "$TEMP/docker29-inspect.json" >"$TEMP/repo-tags-false.json"; assert_inspect_rejected "$TEMP/repo-tags-false.json"

# The live pod comparison is also fixture-driven: it accepts only exact tags
# and config digests across the three Kubernetes runtime imageID encodings.
sed -n '/^pod_image_identities_from_json() {/,/^}/p' "$ROOT/scripts/run-kind-e2e.sh" >"$TEMP/pod-identity.sh"
. "$TEMP/pod-identity.sh"
controller_tag='oxideloop-e2e:failpoints-test'
renderer_tag='oxideloop-e2e:production-test'
controller_config_digest="$config_digest"
renderer_config_digest='sha256:3333333333333333333333333333333333333333333333333333333333333333'
wrong_digest='sha256:4444444444444444444444444444444444444444444444444444444444444444'
write_pod_fixture() {
  fixture=$1; controller_id=$2; renderer_id=$3; controller_status=${4:-$controller_tag}; renderer_status=${5:-$renderer_tag}
  cat >"$fixture" <<EOF
{"items":[{"metadata":{"name":"controller-0","uid":"pod-uid"},"spec":{"containers":[{"name":"controller","image":"$controller_tag"},{"name":"renderer","image":"$renderer_tag"}]},"status":{"containerStatuses":[{"name":"controller","image":"$controller_status","imageID":"$controller_id"},{"name":"renderer","image":"$renderer_status","imageID":"$renderer_id"}]}}]}
EOF
}
assert_pod_rejected() {
  if pod_image_identities_from_json "$1" "$controller_tag" "$renderer_tag" "$controller_config_digest" "$renderer_config_digest" >/dev/null 2>&1; then
    echo "invalid pod image identity was accepted: $1" >&2
    exit 1
  fi
}
write_pod_fixture "$TEMP/pod-valid.json" "$controller_config_digest" "$renderer_config_digest"
pod_image_identities_from_json "$TEMP/pod-valid.json" "$controller_tag" "$renderer_tag" "$controller_config_digest" "$renderer_config_digest" >/dev/null
write_pod_fixture "$TEMP/pod-valid.json" "containerd://$controller_config_digest" "containerd://$renderer_config_digest"
pod_image_identities_from_json "$TEMP/pod-valid.json" "$controller_tag" "$renderer_tag" "$controller_config_digest" "$renderer_config_digest" >/dev/null
write_pod_fixture "$TEMP/pod-valid.json" "registry.example/oxideloop@$controller_config_digest" "registry.example/oxideloop@$renderer_config_digest"
pod_image_identities_from_json "$TEMP/pod-valid.json" "$controller_tag" "$renderer_tag" "$controller_config_digest" "$renderer_config_digest" >/dev/null
write_pod_fixture "$TEMP/pod-qualified.json" "containerd://$controller_config_digest" "containerd://$renderer_config_digest" "docker.io/library/$controller_tag" "docker.io/library/$renderer_tag"
pod_image_identities_from_json "$TEMP/pod-qualified.json" "$controller_tag" "$renderer_tag" "$controller_config_digest" "$renderer_config_digest" >"$TEMP/pod-qualified-identity.json"
jq -e '.observed | all(.[]; .imageRelation == "docker.io/library-qualified" and .normalizedImage == .declaredImage)' "$TEMP/pod-qualified-identity.json" >/dev/null
for bad_status_image in "registry.example/$controller_tag" "docker.io/not-library/$controller_tag" "docker.io/library/other:tag" "docker.io/library/oxideloop-e2e:wrong"; do
  jq --arg image "$bad_status_image" '.items[0].status.containerStatuses[0].image = $image' "$TEMP/pod-qualified.json" >"$TEMP/pod-wrong-qualified-image.json"
  assert_pod_rejected "$TEMP/pod-wrong-qualified-image.json"
done
if pod_image_identities_from_json "$TEMP/pod-valid.json" "docker.io/library/$controller_tag" "$renderer_tag" "$controller_config_digest" "$renderer_config_digest" >/dev/null 2>&1; then
  echo 'qualified requested controller tag was accepted' >&2
  exit 1
fi
jq '.items[0].spec.containers[0].image = "wrong:tag"' "$TEMP/pod-valid.json" >"$TEMP/pod-wrong-declared.json"; assert_pod_rejected "$TEMP/pod-wrong-declared.json"
jq '.items[0].status.containerStatuses[0].image = "wrong:tag"' "$TEMP/pod-valid.json" >"$TEMP/pod-wrong-status.json"; assert_pod_rejected "$TEMP/pod-wrong-status.json"
jq --arg digest "$wrong_digest" '.items[0].status.containerStatuses[0].imageID = $digest' "$TEMP/pod-valid.json" >"$TEMP/pod-wrong-digest.json"; assert_pod_rejected "$TEMP/pod-wrong-digest.json"
jq --arg digest "$wrong_digest" '.items[0].status.containerStatuses[0].imageID = ("containerd://" + $digest)' "$TEMP/pod-qualified.json" >"$TEMP/pod-qualified-wrong-digest.json"; assert_pod_rejected "$TEMP/pod-qualified-wrong-digest.json"
jq --arg digest "$wrong_digest" '.items[0].status.containerStatuses[1].imageID = $digest' "$TEMP/pod-valid.json" >"$TEMP/pod-wrong-renderer-digest.json"; assert_pod_rejected "$TEMP/pod-wrong-renderer-digest.json"
jq '.items[0].status.containerStatuses[0].imageID = "not-a-digest"' "$TEMP/pod-valid.json" >"$TEMP/pod-malformed.json"; assert_pod_rejected "$TEMP/pod-malformed.json"
jq 'del(.items[0].status.containerStatuses[0])' "$TEMP/pod-valid.json" >"$TEMP/pod-missing-status.json"; assert_pod_rejected "$TEMP/pod-missing-status.json"
jq '.items[0].status.containerStatuses += [.items[0].status.containerStatuses[0]]' "$TEMP/pod-valid.json" >"$TEMP/pod-duplicate-status.json"; assert_pod_rejected "$TEMP/pod-duplicate-status.json"
jq 'del(.items[0].spec.containers[0])' "$TEMP/pod-valid.json" >"$TEMP/pod-missing-declaration.json"; assert_pod_rejected "$TEMP/pod-missing-declaration.json"
jq '.items[0].spec.containers += [.items[0].spec.containers[0]]' "$TEMP/pod-valid.json" >"$TEMP/pod-duplicate-declaration.json"; assert_pod_rejected "$TEMP/pod-duplicate-declaration.json"
jq '.items += [.items[0] | .metadata.name = "controller-1" | .metadata.uid = "pod-uid-2"]' "$TEMP/pod-valid.json" >"$TEMP/pod-multiple-live.json"; assert_pod_rejected "$TEMP/pod-multiple-live.json"
production_inspect_line=$(rg -n -F 'docker-image-production.inspect.json' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
kind_load_line=$(rg -n -F 'kind load docker-image --name "$CLUSTER_NAME"' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ "$production_inspect_line" -lt "$kind_load_line" ] || { echo 'Docker inspect must be captured before kind loads the tag' >&2; exit 1; }

# These source-level assertions intentionally reject the regressions that a
# command fake cannot model: empty successful captures, list-index crash
# checks, vacuous image checks, and count-only automation/deletion evidence.
for token in \
  'date -u +%Y%m%dt%H%M%Sz' \
  'RUN_ID must contain only lowercase' \
  'exec 9<>"$FIFO"' \
  'wait_for_exact_crash' \
  'CRASH_POD_UID' \
  'docker-image-production.inspect.json' \
  'docker-image-failpoints.inspect.json' \
  'Descriptor.annotations.config.digest' \
  'configDigestSource' \
  'manifestDigest' \
  'Docker inspect cannot prove a config digest distinct from its manifest digest' \
  'Docker inspect Id and Descriptor.digest disagree' \
  'runtime config digests differ from their exact Docker inspect identities' \
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
assert_not_contains 'docker image inspect "$PRODUCTION_IMAGE" --format '\''{{.Id}}'\'''
assert_not_contains 'docker image inspect "$FAILPOINT_IMAGE" --format '\''{{.Id}}'\'''
assert_not_contains 'contains("sha256:")'
assert_not_contains "'.items[0].status.containerStatuses[] | select(.name == \"controller\") | .lastState.terminated.exitCode == 75'"
assert_not_contains "wait_json 'dependency automation materialization' '.items | length == 1' automationrun"
