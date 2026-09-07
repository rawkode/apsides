# Apsides implementation status

Apsides targets specification 0.2. Existing `reakt.dev` identifiers are its active wire names.
The complete production release gate is **not satisfied**. Local compilation,
unit tests, signed disposable candidates, and individual cluster qualifications
establish narrower properties.

## Implemented

### Authoring, compilation, and runtime

- `aps init` creates a starter project with Deno and TypeScript configuration.
  `aps crds fetch` generates ignored, importable types from a pinned CRD source.
  A generated CloudNativePG import was checked through the TypeScript language
  service with zero diagnostics and 55 property completions.
- `cuenv task packageCli` builds a relocatable macOS/Linux CLI prefix with
  compiler dependencies and the native build source closure. Distribution builds
  verify the exact asset tree against a manifest digest embedded in the executable.
  A relocated macOS development-profile bundle passed init, CNPG import, locking,
  compilation and artifact verification with checkout reads denied; six asset
  tamper/removal checks failed without publishing projects. A separate checkout-denied
  build produced the Linux/arm64 controller, OCI image and SBOM. Linux-host execution
  and complete production release acceptance remain unqualified.
- Imported namespaced resources retain their explicit plural, scope and schema
  digest through compilation and deployment review. Force apply remains a
  per-resource choice requiring explicit policy acceptance, including custom
  resources. Managed Secrets remain unsupported; Secret inputs are observed.
- `@reakt/kubernetes` provides the bounded TSX authoring surface for Controllers,
  ConfigMaps, Services, Deployments, declared inputs, and rollout reactions.
- The compiler locks the complete author module closure, typechecks it, evaluates
  it with denied ambient permissions, validates strict canonical IR, and derives
  effects, capabilities, source metadata, and required reactor cases.
- Complete builds produce a Linux controller binary, OCI archive, SBOM, compiler
  closure, build-input record, and integrity manifest.
- The native controller reconciles its fixed resource set through conditional
  Server-Side Apply. It has no JavaScript evaluator and cannot expand its
  approved scope.
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
  This qualifies controller diagnostics for that fixture, not workload health
  or the complete production release gate.

### Release and evidence

- Release manifests bind the compiler closure, native binary, OCI image, SBOM,
  production IR, provenance, signatures, and separately referenced evidence.
- The verifier requires every compiler-generated reactor case and all sixteen
  named release checks. Missing, duplicate, failed, fixture-substituted, or
  unauthenticated production checks reject the release.
- `aps merge-evidence` combines selected receipts against one exact production
  candidate using the existing validators. It copies only referenced files, rejects
  conflicts and duplicate coverage, and reports missing checks without claiming
  release verification. Current accepted coverage is recorded below.
- The deployment-ceiling producer passed on a dedicated kind cluster: removing
  one required capability rejected the exact signed candidate without state
  changes; restoring the policy allowed the same allocation to reach `Planned`.
  Public registry readback and metadata row comparisons passed. No next writer
  started, the temporary cluster and credentials were removed, and existing
  clusters were preserved. This qualifies reservation with the recorded runner;
  it does not execute the candidate binary or qualify GitHub Actions identity.
- The ownership-collision producer passed with two independently signed exact
  candidates on a dedicated kind cluster. The first reserved their shared target;
  the second received the protected claim-collision rejection without changing
  Kubernetes or metadata state. Both generations' writer targets remained absent,
  cleanup preserved existing clusters. One qualification executor was authorized for
  both domains; this does not establish isolation between distinct principals.
- The fixture-promotion producer passed with the exact signed production candidate
  and a real native/OCI fixture release. The fixture matched the signed compiler
  projection and received the exact promotion rejection. The fixture is
  rejected before signature verification; this does not prove a fixture signature
  or qualify runtime deployment.
- Production evidence binds the exact binary, image, IR, compiler closure, build
  inputs, runner source, and diagnostic files. Reactor evidence has a separate,
  narrower fixture contract.
- Reproducibility and declared-input-isolation producers emit only the checks
  they measure. Reactor packaging publishes a closed diagnostic allowlist and
  excludes kubeconfigs, credentials, raw controller logs, and resource values.

### Deployment and protected state

- The deployment CLI verifies the supplied release and protected policy before
  lifecycle work. Immutable OCI retrieval is optional for the new release;
  upgrade recovery does not retrieve an older release.
- Registry schema `reakt.dev/namespace-registry/v8` stores compact transition
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
  immutable records in the configured libSQL metadata store. The Kubernetes
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
which includes the observed-input watch correction. The fresh merger at
`work/cnpg-fourteen-merged-5e63857` accepts **fourteen of sixteen checks and all 43
required reactor cases**. These are the three reactor checks, signed
permission-changing upgrade, reproducible payload, declared-input isolation,
fixture-promotion rejection, deployment-ceiling rejection, ownership
collision, last-type cleanup, finalizer preservation, UID-replacement rejection,
API-bound gap conformance, and uncertain-writer blocking. `releaseVerified`
remains false. Scope-matched benchmark and phase interruption recovery still need
matching evidence. Local qualification signers do not
establish GitHub Actions identity or production release trust.

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
and containers were preserved. The unsigned assembly at
`work/cnpg-corrected-assembled-820dd7c` also passed the exact ignored
`assembled_reactor_evidence_matches_production_contract` consumer test; this is
evidence-contract validation, not release promotion.

Two fresh cache-disabled builds in `work/cnpg-reproducibility-968224c` matched
one another and the corrected signed candidate's binary, OCI image, compiler
closure, and payload identities. `work/cnpg-input-isolation-968224c` compiled the
declared-input control and rejected undeclared imports, clock, random,
environment, filesystem, and network access with the expected reasons. This
measures the declared-module and SES capability boundary, not independent OS
syscall or network isolation. `work/cnpg-fixture-promotion-968224c` verified the
signed production candidate and compiler fixture projection, then required the
specific fixture-promotion rejection. It does not establish runtime behavior.
The unsigned assembly at `work/cnpg-seven-assembled-968224c` passed both exact
reproducibility and input-isolation evidence consumer tests.

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
The reviewed adapter binds the original measurement bytes and all 15 producer
sources; its eleven tests passed. Packaged receipts under
`work/cnpg-cleanup-finalizer-receipt-64f2b39` and
`work/cnpg-cleanup-replacement-receipt-64f2b39` passed the normal merger.

Fresh API-gap qualification at `work/cnpg-api-gap-dab27db` passed all 42
requests for the corrected successor, with no undeclared gaps, unsupported
operations, or outstanding observations. TokenReview authenticated the actual
projected proxy token and its exact Pod and ServiceAccount binding. Required
operations succeeded, out-of-scope operations were denied, and the declared
create/field-scope limitations matched observed behavior. The guarded synthetic
Secret-key experiment also passed and removed its probe key. The receipt binds
all 16 producer sources and the exact signed pair; cleanup restored prior host
inventories and removed credentials. This measures direct Kubernetes API access
with a runtime credential, not proxy-path enforcement or GitHub release trust.

Fresh uncertain-writer qualification at `work/cnpg-uncertain-writer-5e63857`
removed the exact retained terminal predecessor Pod record after Quiesced.
Activation returned the expected protected-writer-history rejection and remained
Quiesced, with identical registry, activation, inventory, and metadata-store
observations before injection and after refusal. The successor runtime stayed
at zero replicas with no runtime Pod; its recorded staged proxy was permitted.
The receipt passed the normal merger for the corrected successor. The run
removed its cluster and credentials and restored prior host inventories. This
qualifies lost termination evidence, not a real network partition or uncertain
stalled-executor takeover.

The following qualification results belong to the **previous candidate**,
`work/cnpg-signed-candidate-c6d67e0`. Its merger at
`work/cnpg-fifteen-merged-7dacabb` accepted fifteen of sixteen checks, missing the
scope-matched benchmark. Those receipts do not qualify the corrected candidate.

The matching native fixture passed all 43 required cases, all 46 fixture cases
(including three reaction-removal controls), and all three reactor checks. Its
synthetic Secret contains the required `uri` key; the harness installs the exact
imported CRD schema. Two independent no-cache builds matched, and declared-input
isolation passed against the same production payload. Fixture execution proves
primitive reactor behavior, not CNPG operator or application readiness.

The previous successor separately passed a signed permission-changing upgrade at
`4892cbc`: initial database readiness, policy rejection without registry changes,
authorized staging, old-writer retirement, activation, commit, and inventory
publication. Live PostgREST responses confirmed consumption of the CNPG-generated
Secret and a ConfigMap-triggered rollout: two rows before the change and one
after, with a new Pod and ReplicaSet and the same Deployment UID. This does not
measure database password rotation or force-enabled custom-resource writes.

Cleanup qualification at `7070c1e` retained the claim while a finalizer delayed
deletion and retained the original claim in Cleaning after replacement-UID
rejection. Both runs preserved an unclaimed Backup and checked that the successor
workload Role omitted Backup access. This Role inspection does not establish
cluster-wide effective authorization. Independent measurement and adapter reviews
passed, with eleven adapter tests.

The uncertain-writer qualification at `d36a7c4` removed the exact retained terminal
predecessor Pod record after Quiesced. Activation returned the expected protected
writer-history rejection, with unchanged registry, activation, inventory, and
metadata-store observations. The successor controller remained at zero replicas
with no runtime Pod; only its recorded staged proxy was allowed. This measures
lost termination evidence, not a real network partition or every possible
uncertain-writer failure.

The API-gap qualification at `1d68c7f` used the active generation proxy's actual
projected token, authenticated its Pod and ServiceAccount binding through
TokenReview, and made 42 direct API requests. Required operations succeeded;
out-of-scope operations were denied; arbitrary-name create and broader-field
patch/update behavior matched the compiler's declared RBAC gaps. Request bodies
and credentials are excluded from the receipt. These probes measure Kubernetes
API authorization with a stolen runtime credential, not proxy-path enforcement.

A guarded synthetic addition to the observed Secret produced a different HMAC
revision, ReplicaSet and ready Pod, with unchanged Deployment UID and workload
image. Existing Secret data stayed equal in memory and ConfigMap resource
versions remained stable; the probe key was removed afterward. This establishes
Secret projection change detection and rollout, not database password rotation.

Each of these live harnesses removed its owned clusters and credentials and
preserved the pre-existing cluster and container inventories. Receipts bind the
exact candidate payload and archived runner sources. They do not establish
production `VerifiedRelease` deployment or qualify a changed compiler or binary.

Automated suites cover strict schema and canonical replay, claim and metadata
store immutability, registry concurrency, capacity accounting, review privacy,
issued-client binding, preview request shape, UID-safe cleanup, finalizer retry,
audit retention, and release assertion/signature rejection. Strict Clippy checks
apply the repository's production-source error-handling rules.

Fresh disposable-cluster qualifications have exercised signed-candidate install
and upgrade, permission-ceiling rejection before reservation, issued credential
reconnection, old-writer credential denial before expiry, adoption, conditional
SSA, finalizer-delayed cleanup, orphan retention, repeated commit, and replacement
UID refusal. Each harness deletes only its owned cluster and removes its
credential files. These runs use the test-candidate authorization boundary and
do not establish production `VerifiedRelease` deployment.

Measured production-payload work has exercised two no-cache builds, declared
input isolation, and the required reactor suite against matching compiler and
runtime inputs. The resulting partial evidence packages pass their dedicated
consumers. They do not contain all required production checks and cannot be
promoted as a complete release.

A test-only interruption hook can pause after an exact validated registry CAS,
allow the parent harness to inspect the protected phase, and remain parked until
the owned child is killed and reaped. It is excluded from production builds.
The six-phase live recovery matrix at producer `7dacabb` passed against the
previous signed CNPG production payload (`c6d67e0`). Each
isolated run kills and reaps the parked deploy process, then uses a fresh issued
client under the same executor binding to complete the recorded transition,
publish inventory, verify repeated commit, and remove its cluster and credentials.
The first three phases use authenticated checkpoint resolution before advancing
the lifecycle; Activated, Cleaning and Committed replay the applicable operation.
All six runs passed independent review, including their 83 frozen inputs,
workload HTTP checks and ConfigMap-driven rollout. The aggregate receipt at
`work/cnpg-phase-recovery-receipt-7dacabb` preserves the original measurements and
passed the normal merger's `phase-interruption-recovery` check.
This covers the validated post-CAS boundary of each phase. It does not prove
within-phase interruption, uncertain stalled-executor takeover, transfer to a
different principal, or GitHub Actions provenance.

Inventory storage has passed a separate local-PV qualification with 2,220,976
bytes of canonical metadata, exact history across Pods, one winner and one
conflict from competing publishers, and recovery after an uncommitted database
writer is killed and reaped. Read-only refusal and healthy reopen also passed.
This covers inventory process termination on the tested local PV; it does not
qualify CSI storage, node failure, or remote Turso. A separate small-fixture
qualification verified current claim, review and audit bodies across two Pods,
with a read-only mount and rejection of all three write methods in the second
Pod. Competing coordination writers also passed: identical claims were idempotent,
distinct reviews coexisted, and one immutable audit candidate won while the other
received the exact conflict error. A fresh read-only Pod verified seed and race
records. This does not prove transaction overlap or cross-store atomicity;
interrupted coordination writes on persistent volumes remain unqualified.

Local statement-failure tests exercise all three public coordination write methods.
An injected failure after insertion must return a static error, leave the new
record absent on reopen, preserve existing references and allow an exact retry.
A raw SQL control verifies that the fault leaves the inserted row inside its
transaction until rollback. These tests cover returned insert errors and cleanup;
they do not qualify process interruption or an uncertain commit outcome.
Separate Unix tests passed SIGKILL and reaping of a raw transaction for each
coordination table with an observed populated rollback journal. Writable recovery
followed by public read-only checks preserved prior records, removed the candidate
and restored deleted synthetic row counts; exact public retries succeeded. This
is local SQLite transaction recovery, not interruption inside the public write
method, persistent-volume failure, remote recovery or cross-store atomicity.

Local filesystem tests cover one libSQL `VACUUM INTO` backup containing all four
stores, exact read-only verification, retained inventory history, exclusion of
later source writes, refusal to overwrite existing destinations, and rejection
of malformed backups without repair.
Inventory supports local and remote read-only access without initialization or repair.
A live two-PVC qualification also passed: the backup-only read-only verifier
recovered original records after all four source stores were changed. Exact Pod
mount sets exclude access to the source during verification. Both local volumes
share one node; this does not establish independent failure domains, node-loss
recovery, or production restore authority.

## Remaining production work

- Complete release qualification for the corrected native candidate. The runtime
  watches only observed inputs, matching compiled permissions; managed inputs
  retain periodic reconciliation. `work/cnpg-input-reactions-89ceec4` passed the
  local signed-pair deployment, permission-changing upgrade, Secret consumption,
  ConfigMap-triggered rollout and idle/active sampling. All 26 runtime health
  samples were ready and synchronized with zero errors. Additional checks passed
  for 30-second equal-value suppression and a synthetic Secret-key update with
  restoration of the original data and workload revision. Database credential
  rotation and the full performance gate remain unqualified.
  Previous release receipts remain bound to the previous binary.
- Produce one signed production release containing every required reactor case
  and all sixteen authenticated release checks.
- Preserve exact payload bindings when assembling the final release. Accepted
  permission-changing upgrade and cleanup receipts for the previous candidate
  do not apply to the corrected binary or compiler closure; those require
  matching new measurements and receipts.
- Keep uncertain stalled-job takeover blocked. A lease, expired token or changed
  executor label alone does not establish that the previous executor cannot
  proceed. The spec includes the trusted deployment job in its trusted computing
  base and forbids requiring a new broadly privileged coordinator.
- Complete claim, review and audit persistent-volume interrupted-write
  qualification, and qualify remote Turso/libSQL restart,
  unavailable-storage, conflict and recovery behavior.
- Qualify byte and time bounds and non-panicking handling of remote responses.
  The store connectors reject non-HTTPS connection URIs, including server-directed libSQL
  requests, but this does not bound response bodies in the pinned upstream client.
- Complete production API authorization and admission qualification for the
  supported Kubernetes versions, including the declared preview permissions and
  request-drain profile.
- Complete scope-matched performance evidence under its production evidence
  contract. Current-candidate workload readiness, last-resource-type cleanup,
  UID replacement and finalizer measurements have passed their stated scopes.
- Define operator recovery for incompatible or corrupt protected state. Current
  schemas fail closed and do not infer missing authority or perform automatic
  conversion.

Specification 0.2 remains the normative target. The contract pages describe the
implemented subset and explicitly identify boundaries that still require live or
production evidence.
