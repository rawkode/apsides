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

assert_contains() { grep -Fq -- "$1" "$ROOT/scripts/run-kind-e2e.sh" || { echo "missing runner contract: $1" >&2; exit 1; }; }
assert_not_contains() { ! grep -Fq -- "$1" "$ROOT/scripts/run-kind-e2e.sh" || { echo "obsolete or vacuous runner contract: $1" >&2; exit 1; }; }

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

# The default-deny collision is proved from the atomically captured container
# termination state, not a rotating `kubectl logs --previous` buffer.
sed -n '/^foreign_collision_termination_from_json() {/,/^}/p' "$ROOT/scripts/run-kind-e2e.sh" >"$TEMP/collision-termination.sh"
. "$TEMP/collision-termination.sh"
collision_failure='controller stopped: core: mutation model rejected desired graph: unsafe desired mutation decision'
write_collision_pod_fixture() {
  fixture=$1; uid=$2; restart=$3; exit_code=$4; message=$5
  jq -n --arg uid "$uid" --argjson restart "$restart" --argjson exitCode "$exit_code" --arg message "$message" '
    {metadata:{name:"controller-0",uid:$uid},status:{containerStatuses:[{name:"controller",restartCount:$restart,lastState:{terminated:{exitCode:$exitCode,message:$message}}}]}}
  ' >"$fixture"
}
assert_collision_rejected() {
  if foreign_collision_termination_from_json "$1" pod-uid 4 "$collision_failure" >/dev/null 2>&1; then
    echo "invalid foreign-collision termination evidence was accepted: $1" >&2
    exit 1
  fi
}
write_collision_pod_fixture "$TEMP/collision-valid.json" pod-uid 4 1 "$collision_failure"
foreign_collision_termination_from_json "$TEMP/collision-valid.json" pod-uid 4 "$collision_failure" >"$TEMP/collision-termination.json"
jq -e '.expected == {podUid:"pod-uid",restartCount:4,exitCode:1,message:"controller stopped: core: mutation model rejected desired graph: unsafe desired mutation decision"} and .observed.container.terminated.messageRelation == "exact"' "$TEMP/collision-termination.json" >/dev/null
write_collision_pod_fixture "$TEMP/collision-newline.json" pod-uid 4 1 "$collision_failure"$'\n'
foreign_collision_termination_from_json "$TEMP/collision-newline.json" pod-uid 4 "$collision_failure" >"$TEMP/collision-newline-termination.json"
jq -e --arg message "$collision_failure" '.observed.container.terminated.message == ($message + "\n") and .observed.container.terminated.normalizedMessage == $message and .observed.container.terminated.messageRelation == "single-trailing-newline-normalized"' "$TEMP/collision-newline-termination.json" >/dev/null
write_collision_pod_fixture "$TEMP/collision-wrong-message.json" pod-uid 4 1 "$collision_failure extra"; assert_collision_rejected "$TEMP/collision-wrong-message.json"
write_collision_pod_fixture "$TEMP/collision-double-newline.json" pod-uid 4 1 "$collision_failure"$'\n\n'; assert_collision_rejected "$TEMP/collision-double-newline.json"
write_collision_pod_fixture "$TEMP/collision-wrong-uid.json" replacement-uid 4 1 "$collision_failure"; assert_collision_rejected "$TEMP/collision-wrong-uid.json"
write_collision_pod_fixture "$TEMP/collision-wrong-restart.json" pod-uid 5 1 "$collision_failure"; assert_collision_rejected "$TEMP/collision-wrong-restart.json"
write_collision_pod_fixture "$TEMP/collision-wrong-exit.json" pod-uid 4 75 "$collision_failure"; assert_collision_rejected "$TEMP/collision-wrong-exit.json"
jq '.status.containerStatuses += [.status.containerStatuses[0]]' "$TEMP/collision-valid.json" >"$TEMP/collision-duplicate-controller.json"; assert_collision_rejected "$TEMP/collision-duplicate-controller.json"
jq '.status.containerStatuses[0].lastState.terminated.message = false' "$TEMP/collision-valid.json" >"$TEMP/collision-message-false.json"; assert_collision_rejected "$TEMP/collision-message-false.json"
jq 'del(.status.containerStatuses[0].lastState.terminated.message)' "$TEMP/collision-valid.json" >"$TEMP/collision-message-missing.json"; assert_collision_rejected "$TEMP/collision-message-missing.json"
jq '.status.containerStatuses[0].restartCount = "4"' "$TEMP/collision-valid.json" >"$TEMP/collision-restart-string.json"; assert_collision_rejected "$TEMP/collision-restart-string.json"

# Managed-fields proof is fixture-driven so the E2E runner cannot accidentally
# accept Update ownership, an incorrect subresource, or a vacuous fieldsV1.
sed -n '/^managed_field_ownership_from_json() {/,/^}/p' "$ROOT/scripts/run-kind-e2e.sh" >"$TEMP/managed-fields.sh"
. "$TEMP/managed-fields.sh"
write_managed_fields_fixture() {
  fixture=$1; subresource=$2; field=$3
  jq -n --arg subresource "$subresource" --arg field "$field" '
    {metadata:{uid:"object-uid",resourceVersion:"42",managedFields:[
      ({manager:"oxideloop-controller",operation:"Apply",fieldsType:"FieldsV1",fieldsV1:{($field):{}}} + (if $subresource == "absent" then {} else {subresource:$subresource} end))
    ]}}
  ' >"$fixture"
}
assert_managed_fields_rejected() {
  if managed_field_ownership_from_json "$1" configmap/root-config main 'f:data' >/dev/null 2>&1; then
    echo "invalid managed-fields ownership was accepted: $1" >&2
    exit 1
  fi
}
write_managed_fields_fixture "$TEMP/managed-config-valid.json" absent 'f:data'
managed_field_ownership_from_json "$TEMP/managed-config-valid.json" configmap/root-config main 'f:data' >"$TEMP/managed-config-assertion.json"
jq -e '.resource == "configmap/root-config" and .manager == "oxideloop-controller" and .operation == "Apply" and .subresource == null and .representativeField == "f:data"' "$TEMP/managed-config-assertion.json" >/dev/null
jq '.metadata.managedFields[0].manager = "other"' "$TEMP/managed-config-valid.json" >"$TEMP/managed-wrong-manager.json"; assert_managed_fields_rejected "$TEMP/managed-wrong-manager.json"
jq '.metadata.managedFields[0].operation = "Update"' "$TEMP/managed-config-valid.json" >"$TEMP/managed-update.json"; assert_managed_fields_rejected "$TEMP/managed-update.json"
jq '.metadata.managedFields[0].subresource = "status"' "$TEMP/managed-config-valid.json" >"$TEMP/managed-main-status.json"; assert_managed_fields_rejected "$TEMP/managed-main-status.json"
jq 'del(.metadata.managedFields[0].fieldsV1["f:data"])' "$TEMP/managed-config-valid.json" >"$TEMP/managed-no-data.json"; assert_managed_fields_rejected "$TEMP/managed-no-data.json"
write_managed_fields_fixture "$TEMP/managed-root-status-valid.json" status 'f:status'
managed_field_ownership_from_json "$TEMP/managed-root-status-valid.json" webapplication/root status 'f:status' >"$TEMP/managed-root-status-assertion.json"
jq -e '.resource == "webapplication/root" and .subresource == "status" and .representativeField == "f:status"' "$TEMP/managed-root-status-assertion.json" >/dev/null
if managed_field_ownership_from_json "$TEMP/managed-root-status-valid.json" webapplication/root main 'f:status' >/dev/null 2>&1; then
  echo 'status managed-fields ownership was accepted as a main-resource apply' >&2
  exit 1
fi

# The restart snapshot has no dependency on list order or resourceVersion; it
# accepts exactly one UID-bearing root, state, ConfigMap, and Service.
sed -n '/^ownership_restart_identities() {/,/^}/p' "$ROOT/scripts/run-kind-e2e.sh" >"$TEMP/ownership-restart-identities.sh"
. "$TEMP/ownership-restart-identities.sh"
STATE_NAME=state-for-root
NAMESPACE=test-namespace
ctx() { cat "$TEMP/ownership-restart-fixture.json"; }
jq -n '
  {items:[
    {kind:"Service",metadata:{name:"root-service",uid:"service-uid"}},
    {kind:"ConfigMap",metadata:{name:"root-config",uid:"config-uid"}},
    {kind:"WebApplication",metadata:{name:"root",uid:"root-uid"}},
    {kind:"OxideLoopState",metadata:{name:"state-for-root",uid:"state-uid"}}
  ]}
' >"$TEMP/ownership-restart-fixture.json"
ownership_restart_identities >"$TEMP/ownership-restart-identities.json"
jq -e '. == {root:{name:"root",uid:"root-uid"},state:{name:"state-for-root",uid:"state-uid"},config:{name:"root-config",uid:"config-uid"},service:{name:"root-service",uid:"service-uid"}}' "$TEMP/ownership-restart-identities.json" >/dev/null
jq 'del(.items[] | select(.kind == "Service"))' "$TEMP/ownership-restart-fixture.json" >"$TEMP/ownership-restart-missing-service.json"
mv "$TEMP/ownership-restart-missing-service.json" "$TEMP/ownership-restart-fixture.json"
if ownership_restart_identities >/dev/null 2>&1; then
  echo 'restart identity snapshot accepted a missing Service' >&2
  exit 1
fi

# The E2E-only overlay must request a durable termination message from the
# controller without changing the production deployment.
rg -Fq 'terminationMessagePolicy: FallbackToLogsOnError' "$ROOT/deploy/e2e/patch-deployment.yaml" || { echo 'missing E2E controller termination message policy' >&2; exit 1; }
! rg -Fq 'terminationMessagePolicy: FallbackToLogsOnError' "$ROOT/deploy/controller/deployment.yaml" || { echo 'termination message policy leaked into the production deployment' >&2; exit 1; }
rendered_e2e=$(kubectl kustomize "$ROOT/deploy/e2e")
printf '%s\n' "$rendered_e2e" | grep -Fq 'terminationMessagePolicy: FallbackToLogsOnError' || { echo 'E2E render lost controller termination message policy' >&2; exit 1; }

production_inspect_line=$(rg -n -F 'docker-image-production.inspect.json' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
kind_load_line=$(rg -n -F 'kind load docker-image --name "$CLUSTER_NAME"' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ "$production_inspect_line" -lt "$kind_load_line" ] || { echo 'Docker inspect must be captured before kind loads the tag' >&2; exit 1; }

recovery_delete_line=$(rg -n -F 'ctx delete configmap root-config -n "$NAMESPACE"' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_annotate_line=$(rg -n -F 'ctx annotate webapplication root -n "$NAMESPACE"' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_config_wait_line=$(rg -n -F "wait_json 'recreated root ConfigMap ownership'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_direct_assert_line=$(rg -n -F 'managed ConfigMap owner reference' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_scale_zero_line=$(rg -n -F 'ctx scale deployment/oxideloop-controller -n "$NAMESPACE" --replicas=0' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_no_pod_line=$(rg -n -F "wait_for_no_controller_pod 'controller deployment scale-to-zero'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_stopped_empty_line=$(rg -n -F 'capture_empty_journal foreign-collision-stopped' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
recovery_scale_one_line=$(rg -n -F 'ctx scale deployment/oxideloop-controller -n "$NAMESPACE" --replicas=1' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ "$recovery_scale_zero_line" -lt "$recovery_no_pod_line" ] && [ "$recovery_no_pod_line" -lt "$recovery_stopped_empty_line" ] && [ "$recovery_stopped_empty_line" -lt "$recovery_delete_line" ] && [ "$recovery_delete_line" -lt "$recovery_annotate_line" ] && [ "$recovery_annotate_line" -lt "$recovery_scale_one_line" ] && [ "$recovery_scale_one_line" -lt "$recovery_config_wait_line" ] && [ "$recovery_config_wait_line" -lt "$recovery_direct_assert_line" ] || { echo 'recovery must scale to zero, prove no pod and empty journal, delete/annotate while stopped, scale up, then wait for exact ConfigMap ownership' >&2; exit 1; }
recovery_wait_line=$(sed -n "${recovery_config_wait_line}p" "$ROOT/scripts/run-kind-e2e.sh")
assert_recovery_wait_contains() { printf '%s\n' "$recovery_wait_line" | grep -Fq "$1" || { echo "recovery ownership wait is missing: $1" >&2; exit 1; }; }
for recovery_owner_field in \
  'apiVersion:"demo.oxideloop.dev/v1alpha1"' \
  'kind:"WebApplication"' \
  'name:"root"' \
  'uid:$rootUid' \
  'controller:true' \
  'blockOwnerDeletion:false' \
  'ownerReferences[]?] == ['; do
  assert_recovery_wait_contains "$recovery_owner_field"
done
recovery_annotate_to_wait=$(sed -n "${recovery_annotate_line},${recovery_config_wait_line}p" "$ROOT/scripts/run-kind-e2e.sh")
if printf '%s\n' "$recovery_annotate_to_wait" | grep -Eq 'wait_json .*webapplication/root'; then
  echo 'root status wait occurs before exact ConfigMap ownership recovery wait' >&2
  exit 1
fi
assert_not_contains "wait_json 'root current render status and durable link'"
assert_contains 'configmap/root-config --arg rootUid "$ROOT_UID"'
assert_contains 'collision recovery reused the foreign ConfigMap UID'
assert_contains 'foreign collision object bytes changed'
assert_contains 'stopped foreign collision object bytes changed'
assert_contains 'wait_for_no_controller_pod'
assert_contains 'controller-scaled-zero-pods.json'
assert_contains 'PreparedNotIssued'
assert_contains 'AfterPrepareBeforeDispatchAuthorization'
assert_contains 'prepared-not-issued-no-post.json'
assert_contains 'postIssued:false'
assert_contains 'PreparedNotIssued invalidation returned to empty journal'
assert_contains 'ambiguous-post-window.json'
assert_not_contains 'arm_crash AfterEventBeforeStatus'
assert_not_contains 'await_crash AfterEventBeforeStatus'
assert_not_contains 'AfterEventBeforeStatus'

# P14-E must capture durable Prepare before the failpoint restart, then prove
# invalidation/replan and identity preservation before ordinary ownership work.
prepared_identity_before_line=$(rg -n -F 'prepared-not-issued-identities-before.json' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prepared_arm_line=$(rg -n -F -- '--point AfterPrepareBeforeDispatchAuthorization' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prepared_reached_line=$(rg -n -F "PreparedNotIssued barrier did not reach REACHED" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prepared_capture_line=$(rg -n -F 'wait_for_prepared_not_issued' "$ROOT/scripts/run-kind-e2e.sh" | tail -1 | cut -d: -f1)
prepared_release_line=$(rg -n -F "PreparedNotIssued barrier did not acknowledge RELEASE" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prepared_restart_line=$(rg -n -F "wait_for_exact_crash 'AfterPrepareBeforeDispatchAuthorization'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prepared_empty_line=$(rg -n -F "wait_json 'PreparedNotIssued invalidation returned to empty journal'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prepared_identity_after_line=$(rg -n -F 'prepared-not-issued-identities-after.json' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_transition_line=$(rg -n -F 'e2e-ownership-transition=$RUN_ID' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ "$prepared_identity_before_line" -lt "$prepared_arm_line" ] && [ "$prepared_arm_line" -lt "$prepared_reached_line" ] && [ "$prepared_reached_line" -lt "$prepared_capture_line" ] && [ "$prepared_capture_line" -lt "$prepared_release_line" ] && [ "$prepared_release_line" -lt "$prepared_restart_line" ] && [ "$prepared_restart_line" -lt "$prepared_empty_line" ] && [ "$prepared_empty_line" -lt "$prepared_identity_after_line" ] && [ "$prepared_identity_after_line" -lt "$ownership_transition_line" ] || { echo 'PreparedNotIssued proof must arm, reach the durable barrier, capture, release, crash, replan to empty journal, preserve identities, then continue' >&2; exit 1; }
assert_contains 'expectedResourceVersion'
assert_contains 'postAuthorized:false'
assert_contains 'postIssued:false'
assert_contains 'requiresIntervention:true'
assert_contains 'prepared-foreign-before.json'
assert_contains 'prepared-foreign-midpoint.json'
assert_contains 'prepared-foreign-after.json'
assert_contains 'PreparedNotIssued midpoint touched or adopted the foreign sentinel'
assert_contains 'PreparedNotIssued restart touched or adopted the foreign sentinel'
after_status_arm_line=$(rg -n -F 'arm_crash AfterStatusBeforeAck' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
after_status_await_line=$(rg -n -F 'await_crash AfterStatusBeforeAck' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ -n "$after_status_arm_line" ] && [ "$after_status_arm_line" -lt "$after_status_await_line" ] || { echo 'AfterStatusBeforeAck recovery failpoint must remain exercised' >&2; exit 1; }

# P13-D must run after initial ConfigMap+Service convergence and before the
# independent prune gate. It captures raw managed-fields documents, records
# their hashes through atomic capture, and restarts while the Service exists.
ownership_transition_line=$(rg -n -F 'e2e-ownership-transition=$RUN_ID' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_config_wait_line=$(rg -n -F "wait_for_same_uid_changed_rv 'ConfigMap ConditionalApply ownership transition'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_service_wait_line=$(rg -n -F "wait_for_same_uid_changed_rv 'Service ConditionalApply ownership transition'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_state_capture_line=$(rg -n -F 'capture_managed_field_ownership state "oxideloopstate/$STATE_NAME" main '\''f:spec'\''' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_config_capture_line=$(rg -n -F 'capture_managed_field_ownership config configmap/root-config main '\''f:data'\''' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_service_capture_line=$(rg -n -F 'capture_managed_field_ownership service service/root-service main '\''f:spec'\''' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_root_capture_line=$(rg -n -F 'capture_managed_field_ownership root-status webapplication/root status '\''f:status'\''' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_restart_line=$(rg -n -F "restart_controller 'ownership transition'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
ownership_service_present_line=$(rg -n -F "wait_json 'ownership restart Service remains present'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
prune_line=$(rg -n -F 'ctx patch webapplication root -n "$NAMESPACE" --type merge -p '\''{"spec":{"expose":false}}'\''' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ "$ownership_transition_line" -lt "$ownership_config_wait_line" ] && [ "$ownership_config_wait_line" -lt "$ownership_service_wait_line" ] && [ "$ownership_service_wait_line" -lt "$ownership_state_capture_line" ] && [ "$ownership_state_capture_line" -lt "$ownership_config_capture_line" ] && [ "$ownership_config_capture_line" -lt "$ownership_service_capture_line" ] && [ "$ownership_service_capture_line" -lt "$ownership_root_capture_line" ] && [ "$ownership_root_capture_line" -lt "$ownership_restart_line" ] && [ "$ownership_restart_line" -lt "$ownership_service_present_line" ] && [ "$ownership_service_present_line" -lt "$prune_line" ] || { echo 'ownership transition must reconcile, capture all main/status ownership, restart with Service present, then prune' >&2; exit 1; }
automation_completion_line=$(rg -n -F "assert_automation_transition 1 \"\$INPUT_UID\" \"\$INPUT_RV\" chocolate" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
automation_ownership_line=$(rg -n -F "capture_managed_field_ownership automationrun-status \"automationrun/\$AUTOMATION_RUN_NAME\" status 'f:status'" "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
automation_capture_line=$(rg -n -F 'capture_transition chocolate' "$ROOT/scripts/run-kind-e2e.sh" | head -1 | cut -d: -f1)
[ "$automation_completion_line" -lt "$automation_ownership_line" ] && [ "$automation_ownership_line" -lt "$automation_capture_line" ] || { echo 'AutomationRun status ownership must be captured after completion and before transition evidence' >&2; exit 1; }
for ownership_contract in \
  'managed_field_ownership_from_json' \
  'atomic_capture_ownership' \
  'ownership-restart-identities-before.json' \
  'prepared-not-issued-state.json' \
  'prepared-not-issued-identities-before.json' \
  'prepared-not-issued-identities-after.json' \
  'ambiguous-post-window.json' \
  'ownership-restart-identities-after.json' \
  'ConfigMap ConditionalApply ownership transition changed its UID' \
  'Service ConditionalApply ownership transition changed its UID' \
  'ownership restart changed root, state, ConfigMap, or Service identities' \
  '--show-managed-fields' \
  'ownershipEvidence:$ownershipEvidence' \
  'missing oxideloop-controller Apply ownership' \
  'f:spec' \
  'f:data' \
  'f:status'; do
  assert_contains "$ownership_contract"
done

# These source-level assertions intentionally reject the regressions that a
# command fake cannot model: empty successful captures, list-index crash
# checks, vacuous image checks, and count-only automation/deletion evidence.
for token in \
  'date -u +%Y%m%dt%H%M%Sz' \
  'RUN_ID must contain only lowercase' \
  'exec 9<>"$FIFO"' \
  'wait_for_exact_crash' \
  'foreign_collision_termination_from_json' \
  'controller-foreign-collision-pod.json' \
  'controller-foreign-collision-termination.json' \
  'foreign collision termination evidence is not exact' \
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
  'ownership-restart-identities-before.json' \
  'automationrun-status' \
  'kind-e2e-reproducibility/v1'; do
  assert_contains "$token"
done
assert_not_contains 'required capture was empty'
assert_not_contains 'map(select(.name == "controller") | .imageID | contains($expected)) | all'
assert_not_contains 'docker image inspect "$PRODUCTION_IMAGE" --format '\''{{.Id}}'\'''
assert_not_contains 'docker image inspect "$FAILPOINT_IMAGE" --format '\''{{.Id}}'\'''
assert_not_contains 'contains("sha256:")'
assert_not_contains 'controller-foreign-collision-previous.log'
assert_not_contains "'.items[0].status.containerStatuses[] | select(.name == \"controller\") | .lastState.terminated.exitCode == 75'"
assert_not_contains "wait_json 'dependency automation materialization' '.items | length == 1' automationrun"
