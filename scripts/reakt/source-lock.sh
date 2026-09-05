#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: scripts/reakt/source-lock.sh ENTRY [MODULE ...]" >&2
  exit 64
fi

repo_root="$(cd "$(dirname "$0")/../.." && pwd -P)"
exec node "$repo_root/packages/bundle-builder/src/reakt-lock.ts" "$@"
