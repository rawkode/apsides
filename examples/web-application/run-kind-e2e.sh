#!/usr/bin/env bash
# Thin example entry point; the repository runner remains authoritative.
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$ROOT"

DIRT=$(git status --porcelain --untracked-files=all)
[ -z "$DIRT" ] || {
  printf '%s\n' "$DIRT" >&2
  printf '%s\n' 'examples/web-application/run-kind-e2e.sh: refusing nonignored working-tree changes' >&2
  exit 1
}

exec "$ROOT/scripts/run-kind-e2e.sh" "$@"
