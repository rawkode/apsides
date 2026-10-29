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
| `aps compile` | validated IR, plan, capabilities, `surface.json`, handler bundles and generated controller source | deployable image or signed release |
| `aps diff` | what changed between two `surface.json` files, and which review-policy rules the change triggers | that the head surface is the one later deployed |
| `aps build` | pinned Linux binary, OCI archive, SBOM and compiler closure | approved provenance or deployment authorization |
| `aps test` | the program's generated semantic scenarios; cases that need a cluster are reported unsupported | cluster behaviour or release trust |
| `aps test --cluster` | every generated reactor case of a fixture release, run against its controller on a disposable, egress-denied kind cluster | production image behaviour, application behaviour or release trust |
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

`aps compile` also writes `surface.json`, the reviewable deployment surface:
permissions, Secrets read, managed resources, inputs, reactions, images,
event handlers, their egress hosts and outputs, risks and the domain's minimum
Kubernetes version. Compare two of them with `aps diff`; see
[Surface diff in CI](/guides/surface-diff/). Each declared
[event handler](/guides/event-handlers/) is bundled into
`controller/src/handlers/` and embedded in the controller binary.

Every output directory must be fresh. A build profile is not checked in: prepare
one with the exact fields documented in [OCI packaging](/reference/oci-packaging/#complete-cli-build).
The complete build also requires podman or Docker with the pinned builder and
runtime images in its local image store, offline Cargo inputs and the configured
`cargo-cyclonedx` executable. It uses podman when `podman` is on `PATH` and
Docker otherwise; set `APSIDES_CONTAINER_ENGINE` to `podman` or `docker` to
choose. `aps` prepares the build context, assembles the image and writes the
SBOM and compiler closure itself; only the cargo compile runs in a container,
from the builder image with no network. The controller embeds V8 through
`deno_core`; the build fetches the pinned `rusty_v8` static archive once,
verifies its SHA-256, and records it in the build inputs and SBOM, because the
compile has no network. See
[OCI packaging](/reference/oci-packaging/#the-pinned-rusty_v8-archive). It emits
`build-result.json`, `image/controller.oci.tar`,
`linux-binary/apsides-controller`, `sbom/sbom.json` and `build-inputs.json`,
and keeps the build context beside the output as `<output>.native-context`.

By default the compile reuses a cargo target directory cached under
`$APSIDES_BUILD_CACHE`, else `$XDG_CACHE_HOME/apsides/build`, else
`~/.cache/apsides/build`, so a rebuild recompiles only what changed.
`--cache disabled` compiles in a fresh target directory that is removed
afterwards; only such builds count as reproducibility evidence. The cache is
not pruned automatically.

## Run the reactor tests on a cluster

`aps test --cluster` runs every generated reactor case against the program's
own controller. It takes a fixture release — `aps build --mode fixture`
assembled with `aps release` — and an OCI archive of the workload fixture image
built from `scripts/fixture-image`:

```sh
# podman; with Docker, tag apsides-fixture:local and run `docker image save`.
podman build --pull=never --network=none --tag docker.io/library/apsides-fixture:local scripts/fixture-image
podman image save --format oci-archive --output work/fixture-image.tar docker.io/library/apsides-fixture:local

target/debug/aps test --cluster \
  --release path/to/fixture-release \
  --fixture-image work/fixture-image.tar \
  --output work/reactor-results
```

It creates a kind cluster named `aps-test-…` from the pinned Kubernetes v1.34.3
node image, with its kubeconfig in a private temporary directory, and never
reads your current context. The node image and
`nginxinc/nginx-unprivileged:alpine@sha256:d908…` (the controller's loopback
proxy, the egress sentinel and the probe Pods) must already be pulled; nothing
is fetched during the run. Images are imported into the node by digest, and the
controller runs in fixture mode with a token for its generated `ServiceAccount`
that expires shortly after the run's own deadline. Programs that manage custom
resources pass each CRD manifest with its own `--crd` flag.

kind runs on podman when `podman` is on `PATH` and on Docker otherwise; set
`APSIDES_CONTAINER_ENGINE` to `podman` or `docker` to choose. The node and
proxy images must be in that engine's image store, and
`scripts/reactor_fixture.sh` builds the workload fixture image with the same
engine. Rootless podman works: the offline gate then uses the node's nft
`iptables` backend, because the legacy one is unavailable there.

Before creating the cluster the runner requires the host kernel to list
`landlock` in `/sys/kernel/security/lsm`: kind nodes share the host kernel, and
every generated controller locks itself down with Landlock at startup. The
controller runs in fixture mode, which checks its embedded event handler
bundles but does not execute handlers, so these cases do not exercise handler
behaviour.

### The offline gate

After the images are imported, the runner blocks egress inside every kind node
container with an `iptables` chain placed first in `OUTPUT` and `FORWARD`. It
lets through loopback, the node's own addresses, established connections and
the cluster's Pod (`10.244.0.0/16`) and Service (`10.96.0.0/16`) ranges, and
rejects everything else. It also refuses Docker's embedded DNS resolver, and
podman's resolver on the network gateway falls to the final `REJECT`, so
external names do not resolve. This applies to the node's containerd and
kubelet and to every Pod. Deny-all `NetworkPolicy` then isolates the workload
and control namespaces, and the controller may reach only the API server.

Each claim is a named probe in `results.json`. A probe that must fail has a
positive control that uses the same method and must succeed:

| Probe | From | Expected |
| --- | --- | --- |
| `node-external-tcp` | node | blocked, and the chain's `REJECT` counter must move |
| `node-external-dns` | node | blocked |
| `node-registry-pull` | node, `crictl pull` of an image it lacks | blocked |
| `node-api-server-service` | node, same TCP method | reachable |
| `pod-sentinel-before-policies` | Pod, before any policy | reachable |
| `pod-sentinel-by-dns`, `pod-api-server-service` | Pod without a policy, after policies apply | reachable |
| `pod-external-tcp`, `pod-external-dns` | Pod without a policy | blocked |
| `workload-pod-sentinel` | Pod in the workload namespace | blocked, retried for up to a minute because the CNI applies a `NetworkPolicy` to a new Pod only after it sees that Pod; the result records the attempts |
| `workload-pod-external-tcp` | Pod in the workload namespace | blocked |
| `controller-sentinel`, `controller-external-tcp` | the controller Pod's proxy | blocked |
| `node-external-tcp-after-cases` | node, after the last case | blocked |

The gate fails if any probe is missing or does not match. It isolates the
cluster from the network; it does not isolate the container host.

### Results and exit status

The output directory holds:

- `results.json`: the report (`apsides.dev/reactor-report/v1`), described below.
- `junit.xml`: one test case per generated case.
- `controller.log`, `egress-canary.log` and `diagnostics/`: controller logs,
  including restarted containers, and object state (never Secrets).
- `semantic-results.json`, `test-seed.json` (input keys, never values) and
  `canary-scan.json`.
- `kubeconfig`: with `--keep-cluster` only, the kept cluster's admin
  kubeconfig, readable only by you.

Before anything is written, every file is scanned for the synthetic secret
values and the fixture rollout key, and any occurrence is replaced by
`[REDACTED]`. The scan counts occurrences and any canary too short to search
for, and fails the run if it finds a leak.

`results.json` records:

| Field | Contents |
| --- | --- |
| `status`, `clean`, `failure` | `passed`, `failed` or `interrupted`; `clean` only when it passed and the cluster was deleted; `failure` is `cases`, `infrastructure` or `interrupted` |
| `runner` | `aps` version, kind version, the container engine and its version, the pinned node image and its local image ID, the proxy image and its config digest |
| `artifact` | release, fixture IR, production IR and overlay digests, the controller image and its config, and the workload fixture image config |
| `cluster` | name, disposition (`deleted`, `kept`, `delete-failed` or `not-created`), Pod and Service ranges, and the exported kubeconfig when kept |
| `isolation` | whether the node rules are installed, every network probe, the controller credential check, image import and the token lifetime |
| `checks` | `offline-gate`, `controller-credential`, `cluster-cleanup`, `reactor-cases`, `secret-canary-scan` and `reaction-mutation-check`, each with typed assertions |
| `summary` | case counts by status |
| `cases` | per generated case: reaction, source location, scenario, layer, inputs, fixture, release and overlay digests, status, evidence, typed assertions and duration |
| `error`, `limitations` | the run-level error, if any, and what the run does not establish |

The reaction-mutation check applies each mutation to the independent revision
oracle's model: the reaction's contribution removed, and each input only that
reaction selects ignored. The live revision must differ from every mutated
expectation. The controller itself is not mutated.

`aps test --cluster` prints progress to stderr and exits with:

| Code | Meaning |
| --- | --- |
| 0 | every check passed; with `--keep-cluster` the run is reported as not clean |
| 1 | a reactor case, the canary scan or the mutation check failed |
| 2 | the command line was invalid |
| 3 | infrastructure: the container engine, kind, images, isolation, credentials or cleanup failed, so the cases prove nothing |
| 130 | interrupted by SIGINT or SIGTERM; the cluster is still deleted and the report written |

The cluster is deleted at the end, including after an interruption, unless
`--keep-cluster` is given. `cuenv task reactorClusterTest` runs the whole path
on `examples/web-application`.

OCI packaging is optional distribution. `aps build` produces an OCI archive
as part of its current complete artifact, but upgrades do not use historical
OCI retrieval as their state store.

`aps deploy` verifies payload integrity and signed provenance against the
platform trust policy before checking live permissions and executing the
lifecycle. `aps verify-release` runs the same release checks without a cluster,
so a signing workflow can check a release before handing it to deployment. A
release carries no test results: run `aps test` for the program before signing
it.

### Sign a release

`aps release` assembles an unsigned release directory. CI signs it with
Sigstore by attesting the release's `release.json` and writing the Sigstore
bundle into the release as `provenance.sigstore.json`. The bundle must hold a
DSSE envelope with an in-toto SLSA v1 provenance statement whose subject is
named `release.json` and carries its SHA-256 digest, signed keylessly by the
CI workload. On GitHub Actions, `actions/attest-build-provenance` with
`subject-path` pointing at the release's `release.json` produces such a
bundle; copy the file its `bundle-path` output names into the release. Any
signer works as long as the subject name is exactly `release.json`.

Apsides never signs and runs no external program to verify: `aps deploy` and
`aps verify-release` verify the bundle in-process and offline against the
trusted root in the platform trust policy.

### Trust policy

The platform stores one trust policy per domain (see
[platform contract](/reference/platform-contract/)); `aps verify-release
--trust-policy` reads the same document from a file:

```json
{
  "schemaVersion": "apsides.dev/trust/v2",
  "repository": "https://github.com/OWNER/REPO",
  "approvedCompilerDigests": ["sha256:…"],
  "signature": {
    "trustedRoot": { "mediaType": "application/vnd.dev.sigstore.trustedroot+json;version=0.1", "…": "…" },
    "transparencyLog": true,
    "certificateTransparency": true,
    "certificateIdentity": "https://github.com/OWNER/REPO/.github/workflows/release.yml@refs/heads/main",
    "certificateIssuer": "https://token.actions.githubusercontent.com",
    "signerDigest": "<commit of the signing workflow>",
    "sourceRef": "refs/heads/main"
  }
}
```

A release verifies only when all of these hold:

- the bundle verifies under `trustedRoot`: the certificate chains to one of
  its certificate authorities and was valid at a signing time proven by a log
  entry or an RFC 3161 timestamp from that root;
- with `transparencyLog`, the bundle's transparency log inclusion proof
  verifies against the root's logs; with `certificateTransparency`, the
  certificate's signed certificate timestamp verifies against the root's
  certificate transparency logs. Public-good Sigstore supports both. GitHub's
  Sigstore instance for private repositories supports neither and timestamps
  every signature instead, so its policy sets both to `false` and its root must
  list timestamp authorities. A root without the authority a setting needs is
  rejected;
- the certificate's subject alternative name and build signer URI equal
  `certificateIdentity`, and its OIDC issuer equals `certificateIssuer`;
- its build signer digest equals `signerDigest`, its source repository equals
  `repository`, its source digest equals the commit `release.json` records,
  its source ref equals `sourceRef`, and it was issued to a provider-hosted
  runner, not a self-hosted one. A certificate that lacks any of these claims
  is rejected;
- `release.json` names `repository` and one of `approvedCompilerDigests`.

The policy digest recorded when a release is staged is the digest of this
document as stored, and activation refuses a policy that changed in between,
so change a domain's trust policy only when no deployment is staged.
`aps verify-release` reports which of the two transparency checks applied.

`aps deploy` also binds the deployment to the exact surface that was
reviewed: it independently recomputes the deployment surface from the
verified release's own IR and requires that digest to equal both the
release's recorded `surface.json` digest and the `surfaceDigest` the
reviewed, authenticated metadata carries. Either mismatch fails the deploy
with a clear error naming the disagreeing digests; there is no override
flag. This also covers a resumed transition: resume revalidates the exact
authenticated review metadata against a fresh projection of the plan, so a
surface that changed after review is refused there too.

## Deployment inputs

`aps deploy` reads `dist/apsides` by default or accepts exactly one current
artifact source:

```sh
aps deploy --artifact path/to/verified-release
# or
aps deploy --artifact-reference path/to/immutable-reference.json
```

An immutable reference resolves the new release with the built-in OCI registry
client and still performs complete trust-policy verification. It does not
authorize tags or make OCI mandatory. Registry credentials and an extra CA come
only from `APSIDES_REGISTRY_USERNAME`, `APSIDES_REGISTRY_PASSWORD` and
`APSIDES_REGISTRY_CA`; without credentials the registry is accessed
anonymously.

The trusted CI job must also provide:

- Kubernetes authentication for an HTTPS API endpoint whose server version is
  at least the domain's `minKubernetesVersion` (`1.34` unless the program
  raises it); `aps deploy` reads the server's discovery version and refuses an
  older server before any lifecycle work;
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
values out of its output. It also lists the program's `surfaceDigest`, handlers,
egress hosts and outputs next to permissions and secrets, so the reviewer sees
exactly which egress hosts, handlers, outputs, secrets and permissions they are
approving. The final result retains the captured review, and an
already-committed retry performs no live preview. See the
[deployment review contract](/reference/review-contract/) for evidence limits;
the acceptance suite runs a live preview of one absent `ConfigMap`.

## What privileged CI owns

The deployer creates generation-specific Roles and RoleBindings, deploys the
new controller image, activates it, and retires the previous writer. The runtime
controller receives only approved reconciliation capabilities. Kubernetes RBAC
is additive, so exact checks of Apsides-owned Roles do not prove total effective
authority on a cluster.

## Current deployment boundary

The [acceptance suite](/reference/acceptance/) deploys signed releases of the
examples with `aps deploy` on disposable kind clusters: install, ceiling
rejection, upgrade with writer quiescence and identity retirement, resume after
each stored phase, finalizer-delayed cleanup and replacement-UID refusal. It is
not production acceptance. Persistent volume recovery, within-phase
interruption, cross-executor recovery, and independent final review remain
open.

Read the [implementation status](/reference/status/) before repeating any
qualification claim. The canonical protocols are [Deployment contract](/reference/deployment-contract/),
[Deployment permissions](/reference/deployment-permissions/), and
[Artifact resolution](/reference/artifact-resolution/).
