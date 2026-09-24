#!/bin/sh
# Compiles the CNPG initial and upgraded examples and exercises `aps diff`
# between them: a clean diff needs no approval, and a policy requiring
# `permissions.added` must require approval, since the upgraded example adds
# permissions (the API Deployment and Service) the initial, database-only
# program never needed.
#
# Generates the CNPG types itself, the same way `cnpgInitialTypes` and
# `cnpgTypes` do, rather than depending on those cuenv tasks: see
# `acceptancePrepare` in env.cue for why (a dependency chain two tasks deep
# makes cuenv's module evaluation time out).
#
#   sh scripts/surface_diff_check.sh <work directory>
set -eu

work=${1:-}
if [ -z "$work" ]; then
  echo "usage: sh scripts/surface_diff_check.sh <work directory>" >&2
  exit 2
fi
aps=${APSIDES_CLI:-target/debug/aps}
cnpg_crd=https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml

rm -rf "$work"
mkdir -p "$work"

deno run --allow-all packages/compiler/src/crds.ts "$cnpg_crd" Cluster examples/cnpg/initial/.aps/crds/cnpg.ts v1
deno run --allow-all packages/compiler/src/crds.ts "$cnpg_crd" Cluster examples/cnpg/.aps/crds/cnpg.ts v1

"$aps" compile --source examples/cnpg/initial/program.tsx --output "$work/initial-compiled"
"$aps" compile --source examples/cnpg/program.tsx --output "$work/upgraded-compiled"

status=0
"$aps" diff --base "$work/initial-compiled" --head "$work/upgraded-compiled" --format markdown \
  >"$work/diff-no-policy.md" || status=$?
if [ "$status" -ne 0 ]; then
  echo "surface diff check: aps diff without a policy must exit 0, got $status" >&2
  exit 1
fi
if ! grep -q '+ permissions default/apps/deployments: create(all),list,patch' "$work/diff-no-policy.md"; then
  echo "surface diff check: expected permission line missing from the diff" >&2
  cat "$work/diff-no-policy.md" >&2
  exit 1
fi

printf '{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["permissions.added"]}' \
  >"$work/policy.json"
status=0
"$aps" diff --base "$work/initial-compiled" --head "$work/upgraded-compiled" --format markdown \
  --policy "$work/policy.json" \
  >"$work/diff-with-policy.md" 2>"$work/diff-with-policy.stderr" || status=$?
if [ "$status" -ne 1 ]; then
  echo "surface diff check: aps diff with a permissions.added policy must exit 1, got $status" >&2
  cat "$work/diff-with-policy.stderr" >&2
  exit 1
fi
if ! grep -q 'permissions.added' "$work/diff-with-policy.stderr"; then
  echo "surface diff check: triggered rule not reported on stderr" >&2
  cat "$work/diff-with-policy.stderr" >&2
  exit 1
fi

echo "surface diff check: ok"
