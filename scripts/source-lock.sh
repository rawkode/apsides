#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: scripts/source-lock.sh ENTRY [MODULE ...]" >&2
  exit 64
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
exec deno run --allow-all "$repo_root/packages/compiler/src/source-lock.ts" "$@"
