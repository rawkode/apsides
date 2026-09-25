---
title: Surface diff in CI
description: Post a deployment surface diff as a PR comment and require approval for reviewable changes.
---

`aps diff` compares two `surface.json` deployment surfaces (the reviewable
projection of a compiled program: permissions, secrets read, managed
resources, inputs, reactions, images, known risks, and — since Phase 4 —
declared event handlers, their egress hosts, and their outputs) and reports
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

`requireApproval` draws from a fixed rule vocabulary; an unrecognized name is
rejected. Each rule matches a specific diff condition, never "any change to
this section" unless stated:

| Rule | Matches when |
| --- | --- |
| `egress.added` | a `(host, port, handler)` edge appears in the `egress` section that was not present before — a handler reaching a new host, a new port on an already-reached host, or a new handler declared with egress at all |
| `secretsRead.added` | a `(namespace, name, reader)` row appears that was not present before. Secrets read are keyed by reader as well as by secret, so a second reader added to an already-read secret matches this rule too, not just a wholly new secret |
| `permissions.added` | a new permission target appears, **or** a kept target's authority widens: a new verb granted, a name added to an already-granted verb, or a verb widened from a name-restricted set to every name (`allNames`). A pure narrowing — a verb or a name dropped, or `allNames` narrowed back to specific names — never matches this rule by itself |
| `permissions.changed` | a kept permission target's verb set changed at all, widening or narrowing alike — the section `permissions.added` only watches for growth |
| `images.unpinned` | a newly added image, or a changed image's new (head) reference, has no `@sha256:` digest |
| `kubernetes.changed` | the `kubernetes` section's `minVersion` or `qualifiedOn` differs |
| `handlers.added` | a handler id present in the head `handlers` section was absent from the base — a wholly new event handler. A handler's trigger, inputs or outputs changing on an already-declared handler id is reported in the diff's `handlers.changed`, but does not itself trigger this rule; name a `permissions.added`/`egress.added`-style rule, or review `handlers.changed` directly, if a kept handler's edges must also require approval |
| `resources.removed` | a managed resource present in the base surface is absent from the head surface |
| `risks.added` | a risk identifier appears that was not present before |

A rule never matches on a removal or narrowing it was not written for:
`permissions.added` ignores a dropped verb, and there is deliberately no
`secretsRead.removed`, `permissions.removed`, `handlers.removed` or
`egress.removed` rule in this vocabulary — review what is newly granted, not
what is no longer needed. The `outputs` surface section (a handler's own
written `ConfigMap`s) has no dedicated policy rule: an output only ever
appears alongside the handler that writes it, which `handlers.added` already
covers.

## GitHub Actions: post a sticky diff comment

This job checks out the pull request's base commit separately, builds `aps`
from **that** checkout, compiles both the base and the head program with it,
runs `aps diff` and keeps one running comment on the pull request up to date
with the result — using
[`marocchino/sticky-pull-request-comment`](https://github.com/marocchino/sticky-pull-request-comment),
pinned by commit SHA. `gh api` (`POST/PATCH
/repos/{owner}/{repo}/issues/{number}/comments`) against a comment id
tracked in a prior run works too, if you would rather not add the action.

Build `aps` from the base commit, never from the pull request's head, and
never let the head checkout's own crates or `packages/compiler` sources into
that build. The head commit is exactly the untrusted content under review;
if the same build compiled both surfaces with a head-built `aps`, a pull
request could change `compiler-core`'s surface derivation itself (or the
authoring compiler) and use that change to hide what it grants from its own
diff. Building from base means one fixed, already-reviewed tool derives both
surfaces, so a head-only compiler change can never doctor its own review. A
pinned, independently released `aps` distribution — verified the way `aps
verify-release` checks a signed release — is a stronger choice still, if your
project has one; building from the base commit below is the from-source
fallback.

Read the review policy from that same base checkout too
(`/tmp/base/apsides.surface-policy.json` below), never from the head
checkout. `aps diff` only requires approval for a rule the policy names; a
policy read from head would let a pull request set `requireApproval` to `[]`
(or drop the rule it would otherwise trip) and exit 0 on its own review.

The base build, the base policy and the two `--source` programs are not the
whole trust boundary, though: on `pull_request` (as opposed to
`pull_request_target`), GitHub runs the workflow file itself from the pull
request's own merge ref, not from the base branch. A pull request can
therefore edit `.github/workflows/surface-diff.yml` directly — remove the
approval step, point `--policy` back at its own tree, anything — and that
edited workflow is what actually runs, with no elevated token or secret to
show for it, but full control over what the resulting status check reports.
Treat this gate the way you would any other required check: protect
`.github/workflows/` (and the review policy file, if it lives outside that
directory) with a `CODEOWNERS` entry and required review, so a workflow or
policy change needs the same approval a permissions change would.

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

      # Every later step reads `aps` — and the review policy below — from
      # here, the base commit's own checkout, never from the head checkout
      # above: nothing the pull request changes in the compiler or the
      # policy can affect how either surface is derived or what requires
      # approval. Only the two `--source` programs are read from the head
      # checkout; see the CODEOWNERS note above this snippet for the
      # workflow file's own trust boundary, which this alone does not cover.
      - name: Check out and build aps from the base commit
        run: |
          git worktree add /tmp/base "${{ github.event.pull_request.base.sha }}"
          cd /tmp/base
          cargo fetch --locked
          cargo build --locked -p cli
          deno install --frozen

      - name: Compile the base surface
        run: |
          /tmp/base/target/debug/aps compile \
            --source /tmp/base/examples/web-application/program.tsx \
            --output work/base-surface

      - name: Compile the head surface
        run: |
          /tmp/base/target/debug/aps compile \
            --source examples/web-application/program.tsx \
            --output work/head-surface

      # The default `bash -e` would abort before `echo` on `aps diff`'s
      # non-zero exit (1: policy match, 2: invalid input), so this step turns
      # that off for itself and records the exit code explicitly instead.
      - name: Run aps diff
        id: diff
        run: |
          set +e
          /tmp/base/target/debug/aps diff \
            --base work/base-surface \
            --head work/head-surface \
            --format markdown \
            --policy /tmp/base/apsides.surface-policy.json \
            > work/surface-diff.md
          echo "exit-code=$?" >> "$GITHUB_OUTPUT"

      # Exit 2 means aps diff itself never produced a usable diff (a missing
      # path, a malformed surface or policy, a broken step above) — post
      # nothing rather than a comment that looks like a result but is not.
      - uses: marocchino/sticky-pull-request-comment@5770ad5eb8f42dd2c4f34da00c94c5381e49af88 # v3.0.5
        if: steps.diff.outputs.exit-code == '0' || steps.diff.outputs.exit-code == '1'
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

A pull request from a fork gets a read-only `GITHUB_TOKEN` on `pull_request`
(GitHub downgrades the `pull-requests: write` permission above regardless of
what the workflow declares), so the sticky-comment step fails there with no
comment posted. The gate still fails closed: the job still fails on that
error, `Require approval for a matched policy rule` still runs for a
same-repo pull request and still blocks on a matched rule, and a required
status check on this job still blocks merge either way. Only the
human-readable comment is missing for a fork pull request, never the
approval requirement itself. Do not switch this workflow to
`pull_request_target` to work around that: it would run with base-repo
write access against the fork's own, unreviewed code.

The sticky comment gives reviewers the same Markdown `aps diff` prints
locally, so `git worktree add` and re-running the two `aps compile` steps by
hand reproduces exactly what the job saw. See
[build and deploy](/guides/build-and-deploy/) for what `aps compile` and `aps
build` each establish, and the [reference map](/reference/) for the deployment
review contract a signed release's `aps deploy` performs separately.

This PR-time diff and the deploy-time review are separate mechanisms that end
up checking the same surface two different ways: this workflow's `aps diff`
compares two independently compiled `surface.json` files as a merge gate,
while `aps deploy`'s reviewed `ReviewMetadata` (`apsides.dev/review-metadata/v3`)
carries its own `surfaceDigest` — the sha256 of the canonical `surface.json`
bytes, derived by the same `derive_surface` function — plus its own
`handlers`/`egress`/`outputs` catalogs. `aps deploy` independently recomputes
that digest from the release actually being deployed and requires it to equal
both the release's recorded `surface.json` digest and the reviewed
`surfaceDigest`; a merged PR whose surface diff was approved here still gets
an independent surface-binding check at deploy time, on the exact release
artifact, not on this workflow's compiled copies.
