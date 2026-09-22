#!/bin/sh
# Build what `aps test --cluster` runs for one program: a fixture release and
# the workload fixture image archive, both under a work directory.
#
#   sh scripts/reactor_fixture.sh <program.tsx> <work directory>
#
# `aps build` never pulls, so the builder and runtime images below, and the
# fixture base in scripts/fixture-image, must already be in the local Docker
# image store. It also needs cargo-cyclonedx on PATH and a BuildKit that can
# export OCI archives: the containerd image store, or a docker-container
# builder selected with BUILDX_BUILDER. The release is unsigned; cluster tests
# check its integrity, not its provenance.
set -eu

source=$1
work=$2
aps=${APSIDES_CLI:-target/debug/aps}
builder=rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
runtime=gcr.io/distroless/cc-debian12@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

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

rm -rf "$work/build" "$work/build.source" "$work/build.oci-context" "$work/release" "$work/fixture-image.tar"
mkdir -p "$work"

# The build profile is written per run from the pins above; it is never
# checked in.
profile=$work/apsides.build.json
tool_sha=$(python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$tool")
printf '{"schemaVersion":"apsides.dev/build-profile/v1","builderImage":"%s","runtimeImage":"%s","platform":"%s","sbomTool":"%s","sbomToolSha256":"sha256:%s","sourceDateEpoch":0}\n' \
  "$builder" "$runtime" "$platform" "$tool" "$tool_sha" >"$profile"

"$aps" build --source "$source" --output "$work/build" --profile "$profile" --mode fixture
digest=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["containerimage.digest"])' "$work/build/image/buildkit-metadata.json")
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

docker build --pull=false --network=none --provenance=false --sbom=false \
  --platform "$platform" --tag apsides-fixture:local scripts/fixture-image
docker image save --output "$work/fixture-image.tar" apsides-fixture:local
