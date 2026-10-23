#!/bin/sh
# Build what `aps test --cluster` runs for one program: a fixture release and
# the workload fixture image archive, both under a work directory.
#
#   sh scripts/reactor_fixture.sh <program.tsx> <work directory>
#
# `aps build` and the workload fixture image use the engine `aps test
# --cluster` runs kind on: podman when it is on PATH, Docker otherwise, or the
# one APSIDES_CONTAINER_ENGINE names. `aps build` never pulls, so the builder
# image below, the runtime base in scripts/runtime-base/reference and the
# fixture image's bash base must already be in that engine's store. It also
# needs cargo-cyclonedx on PATH. The release is unsigned; cluster tests check
# its integrity, not its provenance.
set -eu

source=${1:-}
work=${2:-}
if [ -z "$source" ] || [ -z "$work" ]; then
  echo "usage: sh scripts/reactor_fixture.sh <program.tsx> <work directory>" >&2
  exit 2
fi
aps=${APSIDES_CLI:-target/debug/aps}
builder=rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
runtime=$(cat scripts/runtime-base/reference)

case "$(uname -m)" in
  arm64 | aarch64) platform=linux/arm64 ;;
  x86_64 | amd64) platform=linux/amd64 ;;
  *)
    echo "reactor fixture: unsupported host architecture $(uname -m)" >&2
    exit 1
    ;;
esac
if ! tool=$(command -v cargo-cyclonedx); then
  echo "reactor fixture: cargo-cyclonedx must be on PATH" >&2
  exit 1
fi

# aps build publishes the native context read-only.
if [ -d "$work/build.native-context" ]; then
  chmod -R u+w "$work/build.native-context"
fi
rm -rf "$work/build" "$work/build.source" "$work/build.native-context" "$work/release" "$work/fixture-image.tar"
mkdir -p "$work"

# The build profile is written per run from the pins above; it is never
# checked in.
profile=$work/apsides.build.json
tool_sha=$(sha256sum "$tool" | cut -d' ' -f1)
printf '{"schemaVersion":"apsides.dev/build-profile/v1","builderImage":"%s","runtimeImage":"%s","platform":"%s","sbomTool":"%s","sbomToolSha256":"sha256:%s","sourceDateEpoch":0}\n' \
  "$builder" "$runtime" "$platform" "$tool" "$tool_sha" >"$profile"

"$aps" build --source "$source" --output "$work/build" --profile "$profile" --mode fixture
# build-result.json is pretty-printed with two-space indentation.
digest=$(sed -n 's/^  "imageDigest": "\(sha256:[0-9a-f]\{64\}\)",\{0,1\}$/\1/p' "$work/build/build-result.json")
if [ -z "$digest" ]; then
  echo "reactor fixture: $work/build/build-result.json names no image digest" >&2
  exit 1
fi
"$aps" release \
  --artifact "$work/build" \
  --binary "$work/build/linux-binary/apsides-controller" \
  --oci "$work/build/image/controller.oci.tar" \
  --image "localhost/apsides/fixture-controller@$digest" \
  --platform "$platform" \
  --repository https://github.com/rawkode/apsides \
  --commit "$(git rev-parse HEAD)" \
  --build-inputs "$work/build/build-inputs.json" \
  --sbom "$work/build/sbom/sbom.json" \
  --output "$work/release"

engine=${APSIDES_CONTAINER_ENGINE:-}
if [ -z "$engine" ]; then
  if command -v podman >/dev/null 2>&1; then engine=podman; else engine=docker; fi
fi
case "$engine" in
  podman)
    # podman would otherwise name the image localhost/apsides-fixture.
    podman build --pull=never --network=none \
      --platform "$platform" --tag docker.io/library/apsides-fixture:local scripts/fixture-image
    podman image save --format oci-archive --output "$work/fixture-image.tar" docker.io/library/apsides-fixture:local
    ;;
  docker)
    docker build --pull=false --network=none --provenance=false --sbom=false \
      --platform "$platform" --tag apsides-fixture:local scripts/fixture-image
    docker image save --output "$work/fixture-image.tar" apsides-fixture:local
    ;;
  *)
    echo "reactor fixture: APSIDES_CONTAINER_ENGINE must be podman or docker" >&2
    exit 2
    ;;
esac
