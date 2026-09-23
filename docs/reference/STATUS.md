# Apsides implementation status

Apsides targets specification 0.2. Existing `apsides.dev` identifiers are its active wire names.
Apsides is **not ready for production use**. Local compilation, unit tests,
signed disposable candidates, and individual cluster qualifications establish
narrower properties.

## Implemented

### Authoring, compilation, and runtime

- `aps init` creates a starter project with Deno and TypeScript configuration.
  `aps crds fetch` generates ignored, importable types from a pinned CRD source.
  `cuenv task authoringEditorCheck` verifies a fresh CloudNativePG import through
  Deno's language server: nested type and JSX completions, hover documentation,
  zero valid-document diagnostics, and rejection of invalid fields and types.
- `cuenv task packageCli` builds a relocatable macOS/Linux CLI prefix with
  compiler dependencies and the native build source closure. Distribution builds
  verify the exact asset tree against a manifest digest embedded in the executable.
  A relocated macOS development-profile bundle passed init, CNPG import, locking,
  compilation and artifact verification with checkout reads denied; six asset
  tamper/removal checks failed without publishing projects. A separate checkout-denied
  build produced the Linux/arm64 controller, OCI image and SBOM. Linux-host execution
  remains unqualified.
- Imported namespaced resources retain their explicit plural, scope and schema
  digest through compilation and deployment review. Force apply remains a
  per-resource choice requiring explicit policy acceptance, including custom
  resources. Managed Secrets remain unsupported; Secret inputs are observed.
- `@apsides/kubernetes` provides the bounded TSX authoring surface for Controllers,
  ConfigMaps, Services, Deployments, declared inputs, and rollout reactions.
- The compiler locks the complete author module closure, typechecks it, evaluates
  it with denied ambient permissions, validates strict canonical IR, and derives
  effects, capabilities, source metadata, and required reactor cases.
- Complete builds produce a Linux controller binary, OCI archive, SBOM, compiler
  closure, build-input record, and integrity manifest.
- The native controller reconciles its fixed resource set through conditional
  Server-Side Apply. It has no JavaScript evaluator and cannot expand its
  approved scope.
- Protected platform policy can supply registry Secret references for controller
  and proxy image pulls, without granting runtime Secret access. References bind
  to the policy and staged Pod templates; credentials remain platform-owned.
  `work/private-pull-live-9e6896d` qualified an uncached controller pull on a fresh
  kind node: a missing credential rejection, a fresh HTTP 401 while invalid
  credentials existed, then readiness after a guarded update to valid credentials.
  The Pod UID, Deployment template and protected policy stayed unchanged.
  The same run passed CNPG deployment, permission-change rejection, approved
  upgrade, Secret consumption and a ConfigMap-triggered rollout. Cluster,
  container and registry-volume inventories were restored. This is local
  qualification of the native `5afce8a` pair, not production qualification;
  the invalid-phase event does not establish precisely when kubelet consumed
  the changed Secret bytes.
- Runtime diagnostics retain completed-cycle blocked counts and specific reasons
  throughout reconciliation backoff. `/status` includes up to 256 `conditions`
  containing only `resourceId` and `reasonCode`, plus a `fieldOrVersionConflicts`
  count. A Kubernetes 409 is not classified more narrowly as an SSA ownership
  conflict. Operational failures replace those with a pending cache state and
  their own reason, clearing the per-target list and conflict count. `/readyz` stays unavailable while
  degraded; a clean cycle restores readiness. These probes describe the
  controller, not application workload health.
- A signed disposable-candidate live check observed two distinct SSA conflict
  cycles with advancing reconciliation/error counters, `/readyz` returning 503
  and `/livez` returning 200. Relinquishing the foreign field manager restored
  exact data and readiness while preserving resource UID and controller identity.
  This qualifies controller diagnostics for that fixture, not workload health.

### Releases

- A release manifest (`apsides.dev/release/v2`) binds the compiler closure,
  native binary, OCI image, SBOM, production IR, reactor test plan, fixture
  coverage catalogue, and signed provenance. It carries no test results.
- `aps deploy` accepts a release only after checking payload integrity and
  signed provenance against the platform trust policy. `aps verify-release` runs
  the same checks offline. Both refuse fixture releases, and unit tests confirm
  that a correctly signed fixture release is still refused.
- Per-program behaviour is checked by `aps test`, which belongs in the signing
  workflow. On its own it runs the generated semantic scenarios and reports
  scenarios that need a cluster as unsupported, which fails the command.
  `aps test --cluster` runs every generated case of a fixture release on a
  disposable kind cluster (`crates/test-cluster`). Egress is blocked inside the
  kind node as well as by `NetworkPolicy`, and named probes show external
  addresses, names and registry pulls fail while cluster DNS, Services and the
  API server still work. The reaction-mutation check mutates the revision
  oracle's model, not the controller. On `examples/web-application` all 43
  generated cases and the 3 reaction-mutation checks pass locally on
  linux/arm64. It does not yet run in CI. Live
  qualification of Apsides itself is moving from the Python harnesses in
  `tests/` to a Rust acceptance suite that runs in CI on the examples.

### Deployment and protected state

- The deployment CLI verifies the supplied release and protected policy before
  lifecycle work. Immutable OCI retrieval is optional for the new release;
  upgrade recovery does not retrieve an older release.
- Registry schema `apsides.dev/namespace-registry/v8` stores compact transition
  inputs rather than compiled plans. Each domain retains its active committed
  transition and at most one unfinished transition.
- Reservation binds the registry incarnation, monotonic allocation, plan and
  policy commitments, immutable claim and review references, predecessor
  inventory and writer checkpoint, completion-capacity admission, and issued
  executor identity.
- The executor binding records the approved deployment identity, ServiceAccount
  and bound Secret identities, audience, executor label, scope, and original
  credential expiry. It stores no token or Secret body. A resumed session must
  use a currently valid issued credential under the same protected identity.
- Claims, review metadata, transition audits, and resource inventory use typed
  immutable records in the configured SQLite metadata store. The Kubernetes
  registry selects their exact content-addressed references.
- Commit archives sanitized predecessor lifecycle metadata before pruning the
  predecessor record, advances the protected audit head, promotes the new active
  allocation and writer checkpoint, and preserves idempotent committed retry.
- Capacity admission reserves the remaining canonical registry, activation, and
  inventory envelopes before claim publication. It bounds topology, scalar
  sizes, receipts, and resumes; overflow rejects before dependent writes.

### Review and lifecycle safety

- Deployment review v5 compares authenticated current and predecessor metadata,
  including declarations, lifecycle policy, inputs, reactions, permissions,
  compatibility, enforcement limits, and ownership intent. It never reconstructs
  old desired values.
- An explicitly approved issued deployer can capture bounded conditional SSA
  live impact before a fresh reservation or against a resumable pending
  transition. Reports contain path classifications and coarse ownership/server
  evidence, never live values, managed-field keys, selectors, or response bodies.
- Staging starts the generation's protected API proxy while keeping its controller
  Deployment at zero replicas. Upgrade activation requires
  captured predecessor topology, process termination, qualified request drain,
  claim transfer, and protected activation state before starting the successor.
- Cleanup applies the predecessor's recorded delete/orphan policy and requires
  exact UID authority. Finalizers remain Kubernetes-owned, and replacement UIDs
  are not deleted.
- Runtime and deployment permissions are reported separately. Kubernetes RBAC
  remains additive, so the report describes required access rather than total
  effective authority.

## Verified boundaries

The current signed CNPG candidate is `work/cnpg-signed-candidate-9b5097a`,
which includes the observed-input watch correction. Local qualification signers
do not establish GitHub Actions identity or production release trust.

The rebuilt fixture at `work/cnpg-reactor-native-820dd7c` passed all 43 required
cases and all 46 fixture cases in `work/cnpg-reactor-820dd7c-retry-results`.
Its compiler closure matches the corrected production build except for the
expected fixture IR overlay. The harness installed the imported CNPG CRD schema;
this proves fixture reactor behavior, not execution of the exact production
binary. The current candidate separately passed first deploy, privileged
permission-changing upgrade, ConfigMap reaction, equal-data no-rollout, and
synthetic Secret-key reaction and restoration in
`work/cnpg-input-reactions-89ceec4`. Both fresh runs removed their clusters and
credentials; external inventory comparisons confirmed the pre-existing clusters
and containers were preserved.

Two fresh cache-disabled builds in `work/cnpg-reproducibility-968224c` matched
one another and the corrected signed candidate's binary, OCI image, compiler
closure, and payload identities. `work/cnpg-input-isolation-968224c` compiled the
declared-input control and rejected undeclared imports, clock, random,
environment, filesystem, and network access with the expected reasons. This
measures the declared-module and SES capability boundary, not independent OS
syscall or network isolation.

Fresh reservation qualification passed in
`work/cnpg-deployment-ceiling-35187dd` and `work/cnpg-ownership-35187dd`.
Removing a required capability rejected the exact candidate without changing
protected state; restoring the ceiling allowed the same reservation. A distinct
signed owner reserved `default/api-config`, and the corrected contender was
rejected without state changes or a new runtime writer. The existing owner
artifact was verified under its own trust policy; its binary was not executed.
Both harnesses deleted their owned clusters, removed both kubeconfigs, and
confirmed exact preservation of the prior cluster and container inventories.
These checks qualify reservation-time enforcement, not full deployment or
production release trust.

Fresh cleanup runs at `work/cnpg-finalizer-64f2b39` and
`work/cnpg-replacement-64f2b39` upgraded the signed cleanup predecessor to the
corrected successor. The finalizer run retained the original claim in Cleaning
while deletion was blocked, then committed after removal of the finalizer and
the pinned Backup. The replacement run rejected the new UID and retained the
original claim in Cleaning. Both preserved an unclaimed same-type Backup and
checked that the successor workload Role omitted Backup access. That Role
inspection does not establish cluster-wide effective authorization. Both runs
removed their clusters and credentials and restored prior host inventories.

Fresh API-gap qualification at `work/cnpg-api-gap-dab27db` passed all 42
requests for the corrected successor, with no undeclared gaps, unsupported
operations, or outstanding observations. TokenReview authenticated the actual
projected proxy token and its exact Pod and ServiceAccount binding. Required
operations succeeded, out-of-scope operations were denied, and the declared
create/field-scope limitations matched observed behavior. The guarded synthetic
Secret-key experiment also passed and removed its probe key. The run was bound
to all 16 producer sources and the exact signed pair; cleanup restored prior host
inventories and removed credentials. This measures direct Kubernetes API access
with a runtime credential, not proxy-path enforcement or GitHub release trust.

Fresh uncertain-writer qualification at `work/cnpg-uncertain-writer-5e63857`
removed the exact retained terminal predecessor Pod record after Quiesced.
Activation returned the expected protected-writer-history rejection and remained
Quiesced, with identical registry, activation, inventory, and metadata-store
observations before injection and after refusal. The successor runtime stayed
at zero replicas with no runtime Pod; its recorded staged proxy was permitted.
The run removed its cluster and credentials and restored prior host inventories. This
qualifies lost termination evidence, not a real network partition or uncertain
stalled-executor takeover.

The fresh six-phase recovery matrix at producer `b4f0238` passed for Planned,
Staged, Quiesced, Activated, Cleaning, and Committed. Each run interrupted after
an exact persisted phase change, killed and reaped its owned deploy process,
and used a fresh issued session under the same logical owner to recover.
Planned, Staged, and Quiesced used authenticated checkpoint resolution; the later
phases replayed their applicable operation. All six reached Committed, preserved
inventory under repeated publication and commit, passed five workload checks,
and confirmed old active writers were absent. Independent review verified each
run's 16 sources and 69 frozen inputs. Every run removed its cluster and
credentials and restored prior host inventories. This qualifies post-CAS interruption at those boundaries,
not within-phase interruption or takeover of an uncertain stalled executor.

Automated suites cover strict schema and canonical replay, claim and metadata
store immutability, registry concurrency, capacity accounting, review privacy,
issued-client binding, preview request shape, UID-safe cleanup, finalizer retry,
audit retention, and release integrity/signature rejection. Strict Clippy checks
apply the repository's production-source error-handling rules.

Fresh disposable-cluster qualifications have exercised signed-candidate install
and upgrade, permission-ceiling rejection before reservation, issued credential
reconnection, old-writer credential denial before expiry, adoption, conditional
SSA, finalizer-delayed cleanup, orphan retention, repeated commit, and replacement
UID refusal. Each harness deletes only its owned cluster and removes its
credential files. These runs use the test-candidate authorization boundary and
do not establish the public `aps deploy` path for a complete signed release. The
Rust [acceptance suite](/reference/acceptance/), which runs in CI, deploys the
signed CNPG pair with the public `aps deploy`: ceiling rejection before any
successor object, upgrade, old-identity retirement, data preservation, input
rollouts and a secret-canary scan, on single-node kind with a synthetic signer.

A test-only interruption hook can pause after an exact validated registry CAS,
allow the parent harness to inspect the protected phase, and remain parked until
the owned child is killed and reaped. It is excluded from production builds.

Local storage, coordination-write and backup qualification detail lives in
[resource inventory](/reference/inventory/#current-limits) and is not
duplicated here.

## Remaining production work

- The GitHub Actions checks workflow runs existing cuenv build, authoring,
  editor, Rust, harness and documentation tasks with read-only repository
  permissions. Actionlint, independent review and the hosted Linux checks passed
  at `07b5041`. Release signing and external OIDC-to-Kubernetes identity
  configuration and claim qualification remain separate unfinished work.
- Finish live qualification of the corrected native candidate. The runtime
  watches only observed inputs, matching compiled permissions; managed inputs
  retain periodic reconciliation. `work/cnpg-input-reactions-89ceec4` passed the
  local signed-pair deployment, permission-changing upgrade, Secret consumption,
  ConfigMap-triggered rollout and idle/active sampling. All 26 runtime health
  samples were ready and synchronized with zero errors. Additional checks passed
  for 30-second equal-value suppression and a synthetic Secret-key update with
  restoration of the original data and workload revision. Database credential
  rotation and the full performance gate remain unqualified.
- Port the live qualifications above to the Rust acceptance suite, run it in
  CI on the examples, and delete each Python harness once its scenarios pass there.
- Add a release workflow for the `aps` distribution that signs only after the
  acceptance suite passes.
- Keep uncertain stalled-job takeover blocked. A lease, expired token or changed
  executor label alone does not establish that the previous executor cannot
  proceed. The spec includes the trusted deployment job in its trusted computing
  base and forbids requiring a new broadly privileged coordinator.
- Complete claim, review and audit persistent-volume interrupted-write
  qualification, and qualify unavailable-storage, conflict and recovery
  behavior for the local SQLite metadata store.
- Complete production API authorization and admission qualification for the
  supported Kubernetes versions, including the declared preview permissions and
  request-drain profile.
- Complete the performance gate.
  Current-candidate workload readiness, last-resource-type cleanup,
  UID replacement and finalizer measurements have passed their stated scopes.
- Define operator recovery for incompatible or corrupt protected state. Current
  schemas fail closed and do not infer missing authority or perform automatic
  conversion.

Specification 0.2 remains the normative target. The contract pages describe the
implemented subset and explicitly identify boundaries that still require live
qualification.
