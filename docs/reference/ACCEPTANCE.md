# Acceptance suite

The acceptance suite (`crates/acceptance`) accepts the Apsides implementation
against the example programs, as [specification §28](/reference/spec/#28-initial-release-acceptance-criteria)
requires. It deploys signed releases with the public `aps deploy` command on
disposable kind clusters and checks what each §28 row demands. It does not
accept an individual user's release; `aps test` does that before signing.

## Running it

The suite needs Docker, [kind](https://kind.sigs.k8s.io/), `cargo-cyclonedx`
on `PATH` and these images in the local Docker image store, because neither
`aps build` nor kind pulls:

- the builder, `rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922`;
- the runtime base named in `scripts/runtime-base/reference`;
- the kind node, `kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48`,
  the Kubernetes version the absent-only create contract is qualified on.
  `aps deploy` accepts any server at or above a domain's
  `minKubernetesVersion`, but the suite runs only this one.

The host kernel must list `landlock` in `/sys/kernel/security/lsm`: kind nodes
share it, and every controller applies a Landlock lockdown at startup. Each
scenario checks this before creating its cluster.

```sh
cuenv task acceptancePrepare   # build, release and sign the five releases
cuenv task acceptanceTest      # run every scenario, one kind cluster each
cuenv task acceptanceCheck     # Clippy for the suite
```

`acceptancePrepare` generates the CloudNativePG types, builds releases of
`examples/cnpg/initial`, `examples/cnpg`, `examples/cnpg/cleanup`,
`crates/acceptance/programs/cnpg-owner` and
`crates/acceptance/programs/handler-output` with `aps build` and `aps release`,
signs each release's provenance with an OpenSSH key generated for that run
(the private key is deleted once all are signed), and writes a trust policy
approving only that key, the releases' compiler digests and `/usr/bin/ssh-keygen`.
It verifies every release with `aps verify-release`, fetches the CloudNativePG
1.30.0 operator manifest and accepts it only with its pinned SHA-256. It writes
`work/acceptance/inputs.json`, which `acceptanceTest` passes to the scenarios as
`APSIDES_ACCEPTANCE_INPUTS`. A scenario fails, and never skips, when that file is
missing.

CI prepares once in the `acceptance-prepare` job and hands the releases, with
the `aps` that built them, to four `acceptance` matrix jobs, each running one
cuenv task with `--test-threads=1`: `acceptanceUpgradeTest` (P0, uncertain
writer, ownership collision with live preview), `acceptancePhasesTest`,
`acceptanceCleanupTest` and `acceptanceCredentialsTest` (which also runs the
handler scenario).

Each scenario creates its own cluster named `aps-acc-…` with a kubeconfig in a
private temporary directory, and deletes it when it ends, including on failure.
Nothing reads `KUBECONFIG` or `~/.kube/config`. `aps` runs with a cleared
environment holding only the tool allowlist, the scenario's short-lived issuer
kubeconfig and its inventory database.

Results go to `work/acceptance/results/<scenario>/`: `report.json` with every
passed check and its evidence, the `aps` output, controller logs, and on failure
allowlisted diagnostics (Deployments, ReplicaSets, Pods without environments,
Services, `ServiceAccount`s, volume claims, `ConfigMap` names, Roles,
RoleBindings, CNPG Clusters and Events; never Secrets or kubeconfigs). Every
Secret value, rollout key and token the scenario knows is redacted from these
files before they are written. CI uploads only this directory.

## Scenarios and §28 rows

| Scenario (`tests/kind/`) | Check | §28 row |
| --- | --- | --- |
| `upgrade::cnpg_upgrade_through_the_public_cli` | (a) under a ceiling allowing only the initial release, `aps deploy` of it commits | Upgrade to a build with different permissions |
| | (b) `aps deploy` of the successor fails with the ceiling diagnostic for the capability it adds; no successor or next-generation object exists, and the registry's UID, resourceVersion and state, the reserved sequence and the running initial writer are unchanged | Add a capability above the ceiling |
| | (c) after the platform widens the ceiling, `aps deploy` of the successor commits a new transition and generation | Upgrade to a build with different permissions |
| | (d) every RoleBinding and the `ServiceAccount` the old generation recorded for retirement are gone, no RoleBinding names the old identity, and the new `ServiceAccount` has another name and UID | Upgrade to a build with different permissions |
| | (e) a row written before the upgrade reads back afterwards; the CNPG Cluster and every volume claim keep their UIDs | (example data preservation) |
| | (f) a ConfigMap change rolls `PostgREST` out to a new revision, an equal-data write causes no rollout for 30 s, and a Secret change rolls it out and back under the same rollout key | Reconcile unchanged ConfigMap/Secret input repeatedly |
| | (g) no Secret value, rollout key or issuer token appears in `aps` output, the inventory database files or any control-namespace container log | Inspect artifacts, logs, metrics, and inventory |

| Scenario | Check | §28 row |
| --- | --- | --- |
| `phases::every_interrupted_phase_resumes_to_commit` | Upgrades alternate between the initial release and the successor on one cluster. Each `aps deploy` is stopped right after its registry write into one phase (Planned, Staged, Quiesced, Activated, Cleaning, in turn). Rerunning `aps deploy` of the same release with the same executor commits that transition: no new allocation, no unfinished transition, and the same inventory when repeated. Afterwards the new generation's controller and proxy are ready, no Pod of the old runtime account runs, the CNPG Cluster is Ready and, for the successor, the API rolls out | Interrupt deployment after each phase |
| `uncertain::lost_writer_evidence_blocks_activation` | `aps deploy` of the successor is stopped right after its write into Quiesced, before activation transfers any claim. The predecessor controller's retained terminal Pod is then deleted and its `apsides.dev/termination-evidence` finalizer removed. Rerunning `aps deploy` fails with `live writer set differs from complete protected Pod history`. The transition stays Quiesced, the successor runtime stays at zero replicas with no Pod, activation still names the draining predecessor, and the registry, activation and inventory `ConfigMap`s and every metadata database row are unchanged | Lose certainty about old writer termination |
| `cleanup::cleanup_waits_for_a_finalizer_then_commits` | From `examples/cnpg/cleanup` to `examples/cnpg`, with a test-owned finalizer on the claimed `Backup`: `aps deploy` deletes it and reports `cleanup remains pending` while the finalizer stays, and is killed. Once the finalizer is removed, the rerun commits the same transition and the `Backup` and its claim are gone | Remove the last managed resource of an API type; Leave a finalizer unresolved |
| `cleanup::cleanup_refuses_a_replacement_uid` | A policy denying the deployer's DELETE of the `Backup` stops cleanup with the original UID pinned in Cleaning. The `Backup` is then deleted and recreated under the same field manager. Rerunning `aps deploy` fails with `cleanup refuses to delete a replacement UID`. The replacement is untouched, the registry is unchanged, and the claim stays Releasing for the original UID | Replace a pending-delete object with a new UID |
| both cleanup scenarios | an unclaimed `Backup` of the same type is untouched, and no workload Role bound to the successor's runtime account grants anything on `Backup`s | Remove the last managed resource of an API type |
| `collision::a_claimed_object_cannot_be_claimed_by_another_domain` | The registry is bootstrapped for two domains. `aps deploy` of `cnpg-owner` commits and claims `default/api-config`. `aps deploy` of the CNPG initial release, another domain declaring the same `ConfigMap`, then fails with `new target collides with a protected claim`. The registry object, every object in the control and workload namespaces, the owner's `ConfigMap` and every metadata database row are unchanged. The contender has no transition and no writer | Deploy a second domain claiming the same object |
| `collision::a_claimed_object_cannot_be_claimed_by_another_domain` (preview) | The owner's deployment identity approves live preview of its one `ConfigMap`. Under a hold that denies every registry update by the deployer, `aps deploy` of the owner runs the preview and is refused at its reservation. The registry state and every `ServiceAccount`, `ConfigMap`, Secret, Service, Deployment, Role and RoleBinding in the control and workload namespaces keep their UID and resourceVersion, and `api-config` does not exist. The report records which metadata database tables the reservation wrote before its denied registry write. Without the hold, the committed report's `review.liveImpact` is `available`, observed `fresh-pre-reservation`, for exactly the approved target | (live preview is read-only; no §28 row) |
| `credentials::the_runtime_credential_holds_exactly_its_declared_capabilities` | (a) Under a policy naming an image pull secret, `aps deploy` of the successor commits, and the controller and proxy Deployments it staged, and the proxy Pod, carry exactly that `imagePullSecrets` list | (platform image pull credentials) |
| | (b) The token the kubelet projected into the running proxy's envoy container is read through the API server into memory only. A `TokenReview` authenticates it as the generation's runtime `ServiceAccount` with the proxy Pod's name and UID, and its claims name the same Pod and expire within the projection's lifetime | Exercise the runtime credential outside scope |
| | (c) With that token, every capability the compiled plan declares succeeds (named get, filtered list and watch, dry-run patch, update and create). Unfiltered lists and watches are denied. Each disclosed RBAC gap behaves as the compiler reports it: a dry-run create under another name, and a dry-run patch and update of a field the program never sets, are allowed. A dry-run apply of every desired object succeeds. Reads in another namespace or of another name, RBAC reads, deletion, patches of the platform's trust and policy objects and token requests are denied. The run requires the demonstrated gaps to equal the declared ones | Exercise the runtime credential outside scope; Attempt a broader field patch within an allowed Deployment |
| | (d) A `SubjectAccessReview` of every distinct request, for the user and groups the `TokenReview` returned, gives the same answer | Exercise the runtime credential outside scope |
| | (e) The disclosed `sensitive-revision-change-observable` gap holds: adding a key to the observed Secret rolls the API out to a new revision and removing it restores the original one | Exercise the runtime credential outside scope (the disclosed profile) |
| | (f), (g) The proxy Pod and `ServiceAccount` are unchanged throughout, and the token, the pull secret and every other Secret value or rollout key appear nowhere in `aps` output, controller logs or the inventory database | Inspect artifacts, logs, metrics, and inventory |
| `handlers::a_handler_commits_exactly_its_owned_keys_and_seeds_across_a_restart` | Deploys `crates/acceptance/programs/handler-output`, whose first handler (no egress) copies every key of the observed `handler-source` `ConfigMap` into its output `handler-derived`, and whose second handler, chained on `handler-derived`, writes that output's whole `data` as one `entries` key of `handler-summary`. (1) The first invocation writes exactly the input's keys, with the `apsides.dev/handler-input-revision` annotation; the handler field manager owns those keys and the annotation, and the reconciler's manager owns the labels. (2) Changing the input drops the key the handler no longer writes and changes the revision. (3) A key another field manager adds survives the next commit and is never co-owned by the handler manager. After each of (1)-(3), `handler-summary` records exactly the `handler-derived` data that step settled on, owned by the handler manager. (4) Deleting the controller Pod leaves both outputs' `resourceVersion` unchanged: the restarted controller seeds both revisions from the annotations and rewrites nothing | Change a handler's trigger input and restart the controller |

### Which interruption points are proven

A hold is a `ValidatingAdmissionPolicy` that denies every update of the
namespace registry by the deployer `ServiceAccount` while the stored registry
(`oldObject`) records the transition at phase P. The write into P is admitted,
because its old object is still at the previous phase. Every later registry
write of that run is denied, so the run cannot persist anything past P. The
scenario polls the registry every 200 ms and sends SIGKILL to `aps` as soon as P
is stored. If the next registry write comes first, the denial makes `aps` exit
with the hold's message, and the scenario records which of the two happened.
Either way, the stop falls somewhere between the write into P and `aps`'s next
registry write. It covers Kubernetes objects `aps` may have created or changed
in between, and any claim rows it published before its denied write. It is not
one fixed instruction. The scenario then requires the registry to stay
unchanged for 5 s before it lifts the hold.

| Stop after the registry write into | Proven by |
| --- | --- |
| Planned, Staged, Quiesced, Activated, Cleaning | `phases::every_interrupted_phase_resumes_to_commit`, live |
| Quiesced, then lost termination evidence | `uncertain::lost_writer_evidence_blocks_activation`, live |
| Cleaning, with cleanup pending on a finalizer | `cleanup::cleanup_waits_for_a_finalizer_then_commits`, live (SIGKILL while `aps` retries) |
| Cleaning, with the cleanup DELETE denied | `cleanup::cleanup_refuses_a_replacement_uid`, live (a separate DELETE hold; `aps` exits on the denial) |
| Committed, before the inventory is published | In process only. No registry write follows Committed, and the inventory is written to the local metadata database, so no admission policy can hold this gap. `deploy::tests::reruns_resume_every_stored_phase_and_republish_a_commit` (`crates/cli`) shows a rerun after every stored phase resumes the transition, and after Committed routes to the report that republishes the inventory. `committed_inventory_persists_metadata_and_retries_after_reopen` (`crates/platform`) shows that inventory publication for a Committed registry with no head succeeds from a freshly opened store, and that repeating it returns the same head |

No scenario stops `aps` in the middle of a phase's Kubernetes writes at a
chosen instruction, and none has another executor take over a stalled
transition.

The reactor rows (generated tests, fixture images, offline execution,
reaction-mutation checks, downtime recovery, watch replay and relist, input
denial, out-of-band edits and field-manager conflicts) are covered by
`aps test --cluster` in the `reactor` CI job. Undeclared evaluation input is
covered by `cuenv task authorCompilerTest`
(`packages/compiler/src/compiler.test.ts`). Fixture promotion is covered by
`crates/release/tests/signatures.rs`: a correctly signed fixture release is
refused by the same verification `aps deploy` runs.

## Reproducibility

"Build the same declared inputs twice" is checked in two halves.

- `compiling_a_program_twice_gives_identical_artifacts`
  (`crates/cli/tests/reproducible_compile.rs`) runs `aps compile` of
  `examples/web-application` into two fresh directories and requires the two
  trees to be identical byte for byte: the IR, the compiled plan, the build
  inputs the native build binds, the generated controller source and lockfile,
  and the manifest of file digests. It runs with the CLI tests on every change.
  It proves the compiler's output does not depend on the output path, the time
  or anything else that differs between two runs on one machine.
- `cuenv task reproducibleBuild` (`acceptance-reproduce`) runs `aps build` of
  the same program twice with `--cache disabled` from one build profile and
  requires every output file to have the same digest, including the controller
  binary, the OCI archive and so the image digest, and the SBOM. It writes the
  digests to `work/reproducible-build/report.json`. The `Nightly` workflow runs
  it once a day and on demand, because two native builds take too long for every
  change. It proves the native build is reproducible on one runner with the
  pinned builder and runtime images; it does not compare builds across machines
  or architectures.

BuildKit only compiles the controller; `aps build` assembles the image itself
from the binary, the pinned runtime base and the profile's `sourceDateEpoch`
(see [OCI packaging](/reference/oci-packaging/)), so the archive does not
depend on the Docker driver, the image store or the wall clock.

## Limits

- One single-node kind cluster per scenario, Kubernetes v1.34.3 only.
- The handler scenario's handler makes no `apsides.fetch` call, so live egress
  is not exercised; the engine's own tests cover `fetch` against local fixture
  servers.
- The release signer is synthetic: an ephemeral OpenSSH key trusted only by the
  scenario's own policy. The GitHub Actions attestation path of the trust policy
  is not exercised.
- The issuer credential is a TokenRequest token minted with the cluster's admin
  kubeconfig, not a Pod-bound token or an external OIDC identity.
- The runtime credential scenario sweeps only the CNPG successor's plan. It
  checks the requests the plan implies and a fixed list of out-of-scope ones,
  not every request the credential could make. Its writes are dry runs.
- The pull-secret check covers the references on the staged writers. The
  kubelet's registry authentication is Kubernetes' job and is not exercised:
  the images are already on the node.
- The preview check stops `aps` at its reservation's registry write. Local
  metadata database writes the reservation makes before that write are
  reported, not held.
- The Envoy environment canary (`crates/platform/tests/envoy/canary.py`) is not
  run. The proxy policy's environment qualification digest names that fact
  rather than a canary result.
- The images the platform and the examples pin by digest (Envoy, curl, the CNPG
  operator, PostgreSQL and PostgREST) are pulled into the kind node from their
  registries during the run, so the run needs registry access. A classic
  (overlay2) Docker image store rewrites manifests on `docker image save`, so a
  host-side copy could not stand in for the pinned digests.
- A hold identifies its transition's phase by matching the phase and the
  transition ID in the stored registry's JSON text. That is exact only because
  a domain has at most one unfinished transition and the registry records a
  phase nowhere else, so each scenario reads the phase back after every stop.
- A downgrade to the initial release orphans the successor's API Deployment
  and Service (their deletion policy is `orphan`). The phase scenario deletes
  them before the next upgrade recreates them.
- The cleanup scenarios check the successor's workload Role rules, not
  cluster-wide effective authorization.
- The platform permissions are the fixed grants the scenario installs; they
  show the deployment works with those grants, not that they are minimal.
- The canary scan covers `aps` output, the inventory database and controller
  logs. Metrics endpoints are not scraped.
