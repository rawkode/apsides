---
title: Surface diff in CI
description: Post a deployment surface diff as a PR comment and require approval for reviewable changes.
---

`aps diff` compares two `surface.json` deployment surfaces (Phase 1's
reviewable projection of a compiled program: permissions, secrets read,
managed resources, inputs, reactions, images and known risks) and reports
what changed between a pull request's base and head commit. An optional
review policy names the diff conditions a human must approve before the
change can be treated as reviewed.

## Command

```sh
target/debug/aps diff \
  --base path/to/base-surface \
  --head path/to/head-surface \
  --format markdown \
  --policy apsides.surface-policy.json
```

`--base` and `--head` each accept either a compiled artifact directory (its
`surface.json` is read) or a `surface.json` file directly. `--format` is
`markdown` (the default, a concise PR-comment body) or `json` (the diff and
the triggered policy rule names, for a script to act on). Without
`--policy`, `aps diff` only ever reports the diff.

Exit status:

| Code | Meaning |
| --- | --- |
| 0 | no policy given, or none of its rules matched the diff |
| 1 | the policy named at least one rule that matched; the triggered rules are printed to stderr and included in `--format json` output |
| 2 | invalid usage, or `--base`, `--head` or `--policy` could not be read or parsed |

## Review policy

`apsides.surface-policy.json` is strict JSON (unknown fields and unknown rule
names are rejected):

```json
{
  "schemaVersion": "apsides.dev/surface-policy/v1",
  "requireApproval": [
    "secretsRead.added",
    "permissions.added",
    "permissions.changed",
    "images.unpinned",
    "kubernetes.changed",
    "resources.removed",
    "risks.added"
  ]
}
```

`requireApproval` draws from a fixed rule vocabulary. `egress.added` and
`handlers.added` are also accepted today for forward compatibility with the
event plane's `egress` and `handlers` surface sections; until those sections
exist, the two rules simply never match.

## GitHub Actions: post a sticky diff comment

This job checks out both commits, builds `aps` once from the head commit (so
base and head are compiled with the same compiler), compiles each commit's
program, runs `aps diff` and keeps one running comment on the pull request
up to date with the result — using
[`marocchino/sticky-pull-request-comment`](https://github.com/marocchino/sticky-pull-request-comment),
pinned by commit SHA. `gh api` (`POST/PATCH
/repos/{owner}/{repo}/issues/{number}/comments`) against a comment id
tracked in a prior run works too, if you would rather not add the action.

```yaml
name: Surface diff
on:
  pull_request:

permissions:
  contents: read
  pull-requests: write

jobs:
  surface-diff:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09 # v5
        with:
          persist-credentials: false
          fetch-depth: 0

      - uses: denoland/setup-deno@22d081ff2d3a40755e97629de92e3bcbfa7cf2ed # v2.0.5
        with:
          deno-version: v2.9.5

      - name: Prepare Rust toolchain
        run: |
          rustup toolchain install
          rustup show active-toolchain

      # One `aps` build, from the head commit, compiles both surfaces: a
      # diff must never blame a compiler difference for a program difference.
      - name: Build aps
        run: |
          cargo fetch --locked
          cargo build --locked -p cli
          deno install --frozen

      - name: Compile the base surface
        run: |
          git worktree add /tmp/base "${{ github.event.pull_request.base.sha }}"
          target/debug/aps compile \
            --source /tmp/base/examples/web-application/program.tsx \
            --output work/base-surface

      - name: Compile the head surface
        run: |
          target/debug/aps compile \
            --source examples/web-application/program.tsx \
            --output work/head-surface

      # The default `bash -e` would abort before `echo` on `aps diff`'s
      # non-zero exit (1: policy match, 2: invalid input), so this step turns
      # that off for itself and records the exit code explicitly instead.
      - name: Run aps diff
        id: diff
        run: |
          set +e
          target/debug/aps diff \
            --base work/base-surface \
            --head work/head-surface \
            --format markdown \
            --policy apsides.surface-policy.json \
            > work/surface-diff.md
          echo "exit-code=$?" >> "$GITHUB_OUTPUT"

      - uses: marocchino/sticky-pull-request-comment@5770ad5eb8f42dd2c4f34da00c94c5381e49af88 # v3.0.5
        with:
          header: surface-diff
          path: work/surface-diff.md

      # Exit 0 is the only outcome that needs no human approval: 1 is a
      # matched policy rule, 2 a broken workflow (bad paths, malformed
      # policy) — neither should read as "approved".
      - name: Require approval for a matched policy rule
        if: steps.diff.outputs.exit-code != '0'
        run: exit 1
```

Replace `examples/web-application/program.tsx` with the pull request's own
program entry point. A monorepo with several programs runs this job once per
program, each with its own `header:` so the sticky comments do not collide.

The sticky comment gives reviewers the same Markdown `aps diff` prints
locally, so `git worktree add` and re-running the two `aps compile` steps by
hand reproduces exactly what the job saw. See
[build and deploy](/guides/build-and-deploy/) for what `aps compile` and `aps
build` each establish, and the [reference map](/reference/) for the deployment
review contract a signed release's `aps deploy` performs separately.
