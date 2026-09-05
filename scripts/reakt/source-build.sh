#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/reakt/source-build.sh SOURCE OUTPUT" >&2
  exit 64
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd -P)"
exec node "$repo_root/packages/bundle-builder/src/reakt.ts" "$1" "$2"
