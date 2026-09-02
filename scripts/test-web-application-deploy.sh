#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SCRIPT="$ROOT/examples/web-application/deploy.sh"

bash -n "$SCRIPT"
test -x "$SCRIPT"
rg -Fq 'pnpm bundle:e2e --' "$SCRIPT"
rg -Fq '.buildBundle.authoredSource == {location:"packages/example-bundle/src",controller:"packages/example-bundle/src/controller.tsx"}' "$SCRIPT"
rg -Fq 'buildBundle.archive.sha256' "$SCRIPT"
rg -Fq 'docker kind kubectl jq pnpm rg shasum' "$SCRIPT"
rg -Fq 'kind create cluster' "$SCRIPT"
rg -Fq 'kind load docker-image' "$SCRIPT"
rg -Fq 'kubectl kustomize deploy/controller' "$SCRIPT"
rg -Fq 'sourceUid == $uid' "$SCRIPT"
rg -Fq 'sourceResourceVersion == $rv' "$SCRIPT"
rg -Fq '.note == "chocolate"' "$SCRIPT"
rg -Fq 'ConfigurationChanged' "$SCRIPT"
rg -Fq 'post-baseline AutomationRuns' "$SCRIPT"
rg -Fq 'post-baseline ConfigurationChanged Events' "$SCRIPT"
rg -Fq 'durable automation baseline' "$SCRIPT"
rg -Fq 'status.state == {rootUid:$rootUid,name:$stateName,uid:$stateUid}' "$SCRIPT"
rg -Fq 'demo.oxideloop.dev/finalizer' "$SCRIPT"
rg -Fq 'runtime.oxideloop.dev/state-finalizer' "$SCRIPT"
rg -Fq 'unlinked root' "$SCRIPT"
rg -Fq 'final-automationruns.json' "$SCRIPT"
rg -Fq 'final-events.json' "$SCRIPT"
! rg -Fq '[.items | length == 1]' "$SCRIPT"
! rg -Fq 'demo.sh' "$ROOT/examples/web-application/README.md"

printf '%s\n' 'web-application deploy static contract passed'
