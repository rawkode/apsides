---
title: Build and deploy
description: Move from TypeScript source to a verified release and CI-owned controller generation.
---

The CLI has separate commands for compiling source, constructing a native image,
checking local artifact integrity, verifying a release under trust policy and
running the deployment protocol.

## Command map

| Command | Establishes | Does not establish |
| --- | --- | --- |
| `aps compile` | validated IR, plan, capabilities and generated controller source | deployable image or signed release |
| `aps build` | pinned Linux binary, OCI archive, SBOM and compiler closure | approved provenance or deployment authorization |
| `aps test` | the program's generated semantic scenarios; cases that need a cluster are reported unsupported | cluster behaviour or release trust |
| `aps verify` | local artifact integrity and recomputed plan | trusted release provenance |
| `aps verify-release` | release integrity and signed provenance under a trust policy, offline | platform admission or live cluster behaviour |
| `aps deploy` | the same release checks, then authorized, resumable lifecycle execution | application workload readiness |

## Compile and build

```sh
cargo build -p cli

target/debug/aps compile \
  --source examples/web-application/program.tsx \
  --output work/apsides-compiled

# First prepare a build profile using the contract below, then:
target/debug/aps build \
  --source examples/web-application/program.tsx \
  --output work/apsides-native \
  --profile /path/to/apsides.build.json

target/debug/aps verify --artifact work/apsides-native
```

Every output directory must be fresh. A build profile is not checked in: prepare
one with the exact fields documented in [OCI packaging](/reference/oci-packaging/#complete-cli-build).
The complete build also requires matching locally cached builder/runtime images,
offline Cargo inputs, Docker BuildKit and the configured `cargo-cyclonedx` executable. It emits
`build-result.json`, `image/controller.oci.tar`,
`linux-binary/apsides-controller`, `sbom/sbom.json` and `build-inputs.json`.

OCI packaging is optional distribution. `aps build` produces an OCI archive
as part of its current complete artifact, but upgrades do not use historical
OCI retrieval as their state store.

`aps deploy` verifies payload integrity and signed provenance against the
platform trust policy before checking live permissions and executing the
lifecycle. `aps verify-release` runs the same release checks without a cluster,
so a signing workflow can check a release before handing it to deployment. A
release carries no test results: run `aps test` for the program before signing
it.

## Deployment inputs

`aps deploy` reads `dist/apsides` by default or accepts exactly one current
artifact source:

```sh
aps deploy --artifact path/to/verified-release
# or
aps deploy --artifact-reference path/to/immutable-reference.json
```

An immutable reference resolves the new release through an approved ORAS tool
and still performs complete trust-policy verification. It does not authorize
tags or make OCI mandatory.

The trusted CI job must also provide:

- Kubernetes authentication for an HTTPS API endpoint;
- platform trust, policy ceiling, generation identity and proxy qualification;
- an executor identity, supplied explicitly for trusted local jobs or derived
  from the supported GitHub Actions run identity;
- persistent inventory configuration; and
- any separately approved ownership intent required for adoption.

The metadata store is a local SQLite database on application-owned persistent
storage. `APSIDES_INVENTORY_DATABASE` must be an absolute path:

```sh
export APSIDES_INVENTORY_DATABASE=/absolute/path/on/persistent-storage/inventory.db
aps deploy --artifact path/to/verified-release
```

Do not use an ephemeral CI filesystem as durable inventory. Connection values
are deployment configuration and never enter inventory snapshots or reports.

The command returns a v5 deployment review. If the platform-owned deployment
identity explicitly enables preview, the same command captures conditional SSA
live impact; no extra CI flag is needed. Fresh observations precede reservation,
while resumed observations are labeled current-state. Missing protected UID or
adoption evidence can make a pending live preview unavailable without granting
any new execution authority. Identity, policy, transport and permission failures
remain errors. Without opt-in, the report explicitly says preview was not approved.

The review lists supplemental preview permissions separately and keeps observed
values out of its output. The final result retains the captured review, and an
already-committed retry performs no live preview. See the
[deployment review contract](/reference/review-contract/) for evidence limits;
actual issued-token SSA qualification remains outstanding.

## What privileged CI owns

The deployer creates generation-specific Roles and RoleBindings, deploys the
new controller image, activates it, and retires the previous writer. The runtime
controller receives only approved reconciliation capabilities. Kubernetes RBAC
is additive, so exact checks of Apsides-owned Roles do not prove total effective
authority on a cluster.

## Current deployment boundary

Focused candidate lifecycle runs have exercised install and upgrade behaviour,
including writer quiescence, adoption, explicit force and finalizer-delayed
cleanup. They are not final production-artifact acceptance. Persistent
volume recovery, within-phase interruption, cross-executor recovery, and
independent final review remain open.

Read the [implementation status](/reference/status/) before repeating any
qualification claim. The canonical protocols are [Deployment contract](/reference/deployment-contract/),
[Deployment permissions](/reference/deployment-permissions/), and
[Artifact resolution](/reference/artifact-resolution/).
