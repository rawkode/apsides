# Apsides implementation status

Apsides implements the current protocol described by specification 0.2. Existing `reakt.dev` identifiers are its active wire names.
The complete production release gate is **not satisfied**. Local compilation,
unit tests, signed disposable candidates, and individual cluster qualifications
establish narrower properties.

## Implemented

### Authoring, compilation, and runtime

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

### Release and evidence

- Release manifests bind the compiler closure, native binary, OCI image, SBOM,
  production IR, provenance, signatures, and separately referenced evidence.
- The verifier requires every compiler-generated reactor case and all sixteen
  named release checks. Missing, duplicate, failed, fixture-substituted, or
  unauthenticated production checks reject the release.
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
- Staging creates a distinct inactive generation. Upgrade activation requires
  captured predecessor topology, process termination, qualified request drain,
  claim transfer, and protected activation state before starting the successor.
- Cleanup applies the predecessor's recorded delete/orphan policy and requires
  exact UID authority. Finalizers remain Kubernetes-owned, and replacement UIDs
  are not deleted.
- Runtime and deployment permissions are reported separately. Kubernetes RBAC
  remains additive, so the report describes required access rather than total
  effective authority.

## Verified boundaries

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
The six-phase live recovery matrix has passed for signed test candidates. Each
isolated run kills and reaps the parked deploy process, then uses a fresh issued
client under the same executor binding to complete the recorded transition,
publish inventory, verify repeated commit, and remove its cluster and credentials.
This covers the validated post-CAS boundary of each phase. It does not prove
within-phase interruption, transfer to a different CI executor, or recovery of a
matching production release payload, and does not satisfy the production
phase-recovery release check.

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
interrupted coordination writes remain unqualified.

Local filesystem tests cover one libSQL `VACUUM INTO` backup containing all four
stores, exact read-only verification, retained inventory history, exclusion of
later source writes, refusal to overwrite existing destinations, and rejection
of malformed backups without repair.
Inventory now supports local read-only access without initialization or repair.
A live two-PVC qualification also passed: the backup-only read-only verifier
recovered original records after all four source stores were changed. Exact Pod
mount sets exclude access to the source during verification. Both local volumes
share one node; this does not establish independent failure domains, node-loss
recovery, or production restore authority.

## Remaining production work

- Produce one signed production release containing every required reactor case
  and all sixteen authenticated release checks.
- Qualify and package Planned, Staged, Quiesced, Activated, Cleaning, and Committed
  interruption recovery against the exact production payload, including safe
  recovery by a different CI executor and `unsafe-activation: false`.
- Define and qualify executor transfer for a different CI run. The current
  executor label and recorded credential expiry do not prove that an earlier
  executor cannot mint credentials, complete admitted requests, or write the
  metadata store.
- Complete claim, review and audit persistent-volume interrupted-write
  qualification, and qualify remote Turso/libSQL restart,
  unavailable-storage, conflict and recovery behavior.
- Complete production API authorization and admission qualification for the
  supported Kubernetes versions, including the declared preview permissions and
  request-drain profile.
- Complete scope-matched performance evidence, workload readiness and status,
  uncertain-writer refusal, ownership collision, last-resource-type cleanup,
  finalizer preservation, fixture-promotion rejection, and API-gap conformance
  under their production evidence contracts.
- Define operator recovery for incompatible or corrupt protected state. Current
  schemas fail closed and do not infer missing authority or perform automatic
  conversion.

Specification 0.2 remains the normative target. The contract pages describe the
implemented subset and explicitly identify boundaries that still require live or
production evidence.
