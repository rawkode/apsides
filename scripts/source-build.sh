#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/source-build.sh SOURCE OUTPUT" >&2
  exit 64
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
exec deno run --allow-all "$repo_root/packages/compiler/src/compiler.ts" "$1" "$2"
