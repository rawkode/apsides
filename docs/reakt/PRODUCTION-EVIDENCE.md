# Production acceptance evidence

A signed candidate is not a verified release. Promotion requires every generated
required reactor case and all sixteen release checks. Missing, failed or
unsupported checks remain errors.

Coverage selects one of two strict evidence contracts by check identity:

- Generated reactor cases and `offline-reactor-suite`, `secret-canary-scan`, and
  `reaction-mutation-check` use `reakt.dev/reactor-evidence/v1`. These establish
  fixture conformance under the declared substitution contract.
- The other thirteen checks use `reakt.dev/production-evidence/v1`. Fixture
  evidence cannot satisfy them, even when a trusted signer signs it.

Production evidence binds the exact IR digest, compiler closure digest, native
binary digest, OCI image reference and platform, and canonical build-input digest.
Its payload file map must exactly match the signed release entries for
`build-inputs.json`, `controller.oci.tar` and `reakt-controller`. The runner source
file and its digest must also be present in the signed release, and verification
rereads and hashes those bytes. Diagnostic references must be signed payload
entries. Unknown fields, unknown checks, fixture checks and malformed assertions
are rejected.

The final release digest is deliberately absent from the evidence subject: the
release envelope contains the evidence, so including that digest would create a
self-reference. The enclosing provenance signature authenticates the receipt.
These are signer-attested measurements. An authenticated runner source file and
assertions alone do not establish that the runner executed.

## CNPG reactor qualification

At source revision `0ee1a75`, the corrected CNPG fixture passed 43 required cases,
46 fixture cases and the three reactor checks. Results and the closed evidence
package are in `work/cnpg-reactor-0ee1a75-results`. The runner used
`work/cnpg-reactor-native-0ee1a75-1` and its `.source` fixture contract, the pinned
workload archive, and `--crd-manifest work/cnpg-demo-inputs/clusters-v1.30.0.yaml`.
That CRD's bytes match the imported type's schema digest. No CNPG operator was
installed; synthetic inputs provide the required Secret keys.

The isolated cluster was deleted and its driver/runtime credentials removed.
Independent review checked exact case sets, input and image hashes, CRD identity,
runner source and packaged diagnostics. This establishes fixture reactor
behavior; the Secret scan covers the controller log and synthetic canaries.

The fresh unsigned production candidate is
`work/cnpg-production-candidate-0ee1a75-1`. Its normal merge summary at
`work/cnpg-reactor-merged-0ee1a75/summary.json` reports zero missing cases and
thirteen missing checks, with `releaseVerified: false`. The dedicated
`reactorEvidenceLive` consumer also passed against that candidate. The previous
CNPG successor's production-check receipts bind a different binary/compiler
payload and are not reused here. Full coverage and approved signing are still
required before release verification.

At `52a5697`, two independent `--cache disabled` builds produced identical complete
identities and matched this candidate. The reproducibility receipt is in
`work/cnpg-reproducibility-52a5697/evidence`. Declared-input isolation also passed,
with its receipt in `work/cnpg-input-isolation-52a5697/evidence`. Baseline
compilations bracket seven probes: a declared module succeeds, while an
undeclared module and clock, randomness, environment, filesystem and network
capabilities reject. This checks source declarations and SES capabilities, not
operating-system confinement or actual network egress.

The normal merger at `work/cnpg-five-merged-52a5697` accepts both receipts with the
reactor evidence: five checks, all 43 required cases, eleven missing checks, and
`releaseVerified: false`. Both dedicated production-evidence consumer tests pass
against the unsigned assembly at `work/cnpg-five-candidate-52a5697`. Independent
reviews rehashed both build identities, candidate bindings, probe outputs and
the complete packaged source/diagnostic files. No generated outputs are tracked.

Fixture-promotion qualification at `c6d67e0` adds the sixth check for this same
payload. Its receipt is in `work/cnpg-fixture-promotion-c6d67e0/evidence`; the normal
merge summary is `work/cnpg-six-merged-c6d67e0/summary.json`. All 43 cases remain
covered, ten checks remain missing, and `releaseVerified` is false. The driver
repeatedly verifies a locally signed production candidate and rejects the exact
native fixture because of its mode. Fixture rejection precedes signature checks;
the fixture is intentionally unsigned. A separate qualification policy approves
the production compiler and temporary signer. The private key was deleted after
signing, and this evidence does not qualify GitHub Actions trust.
Independent review rehashed both artifact closures, source projection, policy,
verifier and driver, and confirmed the receipt binds the current payload.

Deployment-ceiling rejection passed at `fcf3e45` against the same signed CNPG
candidate. The reservation harness installed only the exact declared Cluster CRD
and pinned the existing `default` namespace in its owned kind cluster. The
restricted ceiling rejected reservation without starting a writer; the positive
control exercised reservation under the approved ceiling. The receipt and frozen
CRD/source files are in `work/cnpg-deployment-ceiling-fcf3e45/evidence`.
Independent review rehashed the candidate binding, seven archived sources, driver
and CRD, and checked unchanged denied-state snapshots and the positive control.
The cluster and credentials were removed, and the original cluster inventory was
restored. The normal merger at `work/cnpg-seven-merged-fcf3e45/summary.json` accepts
seven checks and all 43 cases, with nine checks missing and `releaseVerified:
false`. This is reservation qualification, not a CNPG operator deployment.

## Combining measured evidence

`reakt merge-evidence` combines explicit receipts against one structurally valid
production candidate. The candidate establishes the payload identities; this
command does not verify signature trust or perform the measurements again.

Create a JSON list of receipt paths relative to the list file, for example:

```json
[
  "reproducibility/evidence/production-evidence.json",
  "input-isolation/evidence/isolation-evidence.json",
  "reactor/evidence/reactor-evidence.json"
]
```

```sh
cuenv exec -- target/debug/aps merge-evidence \
  --artifact work/candidate \
  --fragments work/fragments.json \
  --output work/merged-evidence
```

The output contains `evidence/`, `coverage.json`, and `summary.json`. Publication
uses exclusive directory creation and never replaces an existing destination.
The directory is visible while files are written; `summary.json` is written last
as the completion marker. Treat an output without that marker as incomplete.
Cleanup after a write failure is best-effort. Only the receipts and their
referenced files are included. Supplemental fixture-case diagnostics in a selected
reactor receipt are validated and retained, but never counted as required coverage.
Identical filenames deduplicate
only when their bytes match; conflicting files, duplicate checks or cases,
mismatched subjects, and invalid assertions reject the merge. Existing release
validators check production and reactor receipts under their separate contracts.

The summary reports missing required checks and cases and always records
`releaseVerified: false`. Partial coverage remains partial. Pass `evidence/` and
`coverage.json` to `reakt release --evidence` and `--coverage`, then sign and verify
the assembled release through the normal trust policy. A successful merge alone
cannot satisfy the production release gate.

## Exact-candidate cluster qualification

Production cluster checks must retain the candidate's signed namespace, domain,
cluster binding and payload. Recompiling with disposable namespace names changes
the subject and cannot qualify the original production candidate.

The reservation-only qualification capability is compiled under platform's
`test-support` feature. It checks the candidate against an immutable marker and
pinned namespace and node identities, then uses the existing authenticated
reservation path and protected policy. It does not grant staging or activation
and does not construct a verified release. The ordinary deployment CLI does not
enable this feature; existing disposable lifecycle guards remain in effect.

The host runner must establish that its random kind cluster name was absent,
successfully create that cluster with a dedicated kubeconfig, and verify its
Docker container and pinned node image before creating the marker. A public run
identifier provides uniqueness, not authorization. Kubernetes observations check
the current target identities; the marker's node image digest remains a host
runner attestation. Neither alone proves that the runner created the cluster.

This capability alone is a prerequisite, not a completed production evidence producer.
The ceiling check still requires a live denial with an actually required
capability removed from protected policy, unchanged registry and immutable store
state, and no writer or RBAC mutation. Restoring the full policy must allow the
same candidate and allocation to reach `Planned`. No production receipt may be
emitted before both the owned-cluster bootstrap and those measurements succeed.

`cuenv task candidateQualificationTest` exercises target rejection with a mocked
Kubernetes API. `cuenv task candidateQualificationArchivedTest` additionally calls
the public reservation method using a signed production candidate selected by
absolute `APS_QUALIFICATION_CANDIDATE` and `APS_QUALIFICATION_TRUST_POLICY` paths.
It verifies the candidate signature before testing against mocked Kubernetes and
metadata stores. This opt-in archived-candidate integration test establishes
neither cluster ownership nor production deployment acceptance.

`tests/reakt/run_deployment_ceiling.py` runs the live reservation experiment. It
requires an existing pinned kind node image, a clean source checkout, and an
independently approved trust policy. It verifies the signed candidate before
creating a new cluster and leaves the candidate's payload and scope unchanged.

```sh
cuenv exec python3 tests/reakt/run_deployment_ceiling.py \
  --artifact "$PWD/work/signed-candidate" \
  --trust-policy "$PWD/work/approved-trust.json" \
  --output "$PWD/work/deployment-ceiling"
```

The runner pins its Docker context and daemon, provisions short-lived deployment
identity, and freezes the protected request before each attempt. A separate
capability-complete projection supplies the list of objects that must remain
absent; it does not authorize the deficient request. The negative control must
return the exact typed capability-ceiling error without changing Kubernetes or
metadata state. Restoring only the protected policy must allow the same candidate
and allocation to reach `Planned`, followed by an exact public-store readback.

Python snapshots and packaged diagnostics never retain Secret bodies. The runner
uses kubectl's metadata-only output, although kubectl itself fetches the object.
SQLite rows are compared in memory as canonical typed values; evidence includes
only hashes and counts. Candidate and runner source commits are recorded
separately. This checks reservation with the recorded runner, not execution of
the candidate's controller binary.

The receipt is written only after cluster/container deletion, preservation of
pre-existing inventories, credential removal, and source revalidation. A failed
run records its cluster name and whether cleanup was confirmed. Changed or
unverifiable container ownership requires manual cleanup and cannot produce a
passing receipt. `cuenv task deploymentCeilingTest` exercises orchestration and
snapshot failures with mocks; it does not qualify a live cluster. Feed successful
receipts through `merge-evidence` and the existing release validators before
counting this check toward production coverage.

## Ownership collision qualification

The `test-support` method `qualify_candidate_collision` is a negative-only
prerequisite for the ownership-collision producer. It accepts two exact verified
candidates in the same scope, with distinct domains and one shared resource
identity. The owner must be the sole untouched `Planned` reservation. Its compiled
plan, frozen inputs, protected policy, and stored review must agree. The contender
must receive the exact claim-collision rejection through authenticated reservation;
registry, policy, review, trust, and disposable-target checks must remain stable.

Pure state tests and compilation alone do not qualify this check. The live
producer has exercised the two-candidate public method, immutable marker
replacement, exact state comparisons, and successful receipt validation. The API
itself emits no receipt and cannot admit a non-colliding second reservation.

`tests/reakt/run_ownership_collision.py` runs that experiment with two signed
production candidates. The contender is the unchanged subject of the resulting
receipt; the owner is independently built from tracked source. Both candidates
must share scope, have distinct domains, and overlap at exactly one canonical
resource identity. The owner reserves first. The runner then conditionally deletes
its immutable marker and creates the contender marker before measuring rejection.

```sh
cuenv exec python3 tests/reakt/run_ownership_collision.py \
  --artifact "$PWD/work/contender-candidate" \
  --trust-policy "$PWD/work/contender-trust.json" \
  --owner-artifact "$PWD/work/owner-candidate" \
  --owner-trust-policy "$PWD/work/owner-trust.json" \
  --output "$PWD/work/ownership-collision"
```

One qualification executor is authorized for both domains. This experiment does
not prove credential isolation between distinct principals. Candidate payloads,
raw state, credentials, and trust bodies are excluded from evidence; the owner is
bound separately by source and payload digests. The same owned-cluster cleanup
and receipt-last rules as the ceiling runner apply. Archived source filenames are
scoped to this check, with a repository-relative source mapping, so older receipts
can retain different versions of shared helpers without filename conflicts.
`cuenv task ownershipCollisionTest` checks orchestration contracts without a live
cluster.

## Reproducibility runner

`tests/reakt/run_reproducibility.py` runs two fresh CLI builds with
`--cache disabled` and compares their
production payloads. It emits only the measured `reproducible-payload` check after
both builds succeed and match; it does not manufacture the remaining coverage.
Use `--help` for the required source, profile, CLI, image repository and output
arguments. The source checkout must be clean and its commit must remain unchanged.
Output belongs in an ignored directory, such as `work/`, and must not be committed.

Run the producer through the configured environment:

```sh
cuenv exec python3 tests/reakt/run_reproducibility.py \
  --source examples/reakt/program.tsx \
  --profile work/build-profile.json \
  --cli "$PWD/target/debug/aps" \
  --image-repository ghcr.io/rawkode/apsides-controller \
  --output work/reproducibility
```

Prepare the build profile and build the CLI with `cuenv task build` first. This
command measures local image archives; it does not push an image to that repository.

Successful runs create a flat `OUTPUT/evidence/` directory containing the receipt,
runner source and two build transcripts. Pass this directory to `reakt release
--evidence`; internal receipt references already use the release's `evidence/`
prefix. Coverage must reference `evidence/production-evidence.json`. Build outputs
remain outside this flat directory.

`cuenv task productionEvidenceLive`, with `APS_PRODUCTION_EVIDENCE_RELEASE` set to
an absolute assembled-release directory, invokes the actual payload and
reproducibility-evidence validators. This opt-in check does not verify signatures
or satisfy the other release checks.

The build profile, pinned tools and local image prerequisites remain the normal
build requirements. A mismatch or build failure is a failed qualification; it is
not permission to discard differing payload fields. Matching builds establish
payload reproducibility for those measured runs. Declared-input isolation remains
a separate mandatory release check.

`cuenv task reproducibilityTest` exercises runner failure handling with synthetic
build processes. `cuenv task releaseTest` exercises signature and evidence
verification. Neither command establishes production build or cluster acceptance.

## Declared-input isolation

`tests/reakt/run_input_isolation.py` measures the authoring boundary against an
already measured production build. It brackets controlled probes with normal CLI
compilations, requiring the same production IR and authoring compiler closure.
The declared-module control and omitted-module probe contain the same physical
dependency; only inclusion in the lock changes. Other probes check SES rejection
of clock/random access and absence of guest environment, filesystem and network
capabilities. Those absence checks do not issue operating-system or network calls.

The probe helper calls the existing compiler API because the public CLI deliberately
hides detailed errors. It classifies only specific errors, rejects unrelated
failures and emits closed result tags. The producer authenticates its Python
source, shared measurement helpers and TypeScript probe source through the flat
evidence directory. The resulting receipt attests only `declared-input-isolation`.

```sh
cuenv exec python3 tests/reakt/run_input_isolation.py \
  --source examples/reakt/program.tsx \
  --artifact work/reproducibility/build-1 \
  --production-evidence work/reproducibility/evidence/production-evidence.json \
  --cli "$PWD/target/debug/aps" \
  --output work/input-isolation
```

Pass the output's flat `evidence/` directory to release assembly. Coverage must
reference `evidence/isolation-evidence.json`. Combining this with another evidence
directory requires preserving every referenced file and rejecting conflicting
copies; copying a receipt alone is insufficient.

`cuenv task inputIsolationTest` checks orchestration with mocked compiler outcomes.
`cuenv task inputIsolationEvidenceLive` reads an actual assembled release selected
by `APS_PRODUCTION_EVIDENCE_RELEASE` and validates this receipt. Release validation
checks the fixed assertion contract and signed file bindings; it does not replay
the probes or independently establish host-level isolation from their result tags.

## Reactor evidence packaging

Successful fresh runs of `tests/reakt/run_cluster.py` stage a flat `OUTPUT/evidence/`
directory for `reakt release --evidence`. Packaged receipts use release-relative
`evidence/` references for the tested artifact manifest and case/check diagnostics.
`OUTPUT/release-coverage-fragment.json` carries matching receipt references; it
selects the production IR and remains a reactor fragment to merge into the
complete release coverage report.
The raw local reports retain their original local references.

Staging includes only referenced diagnostics from a fixed allowlist. Missing,
symlinked, oversized or unapproved files reject before the staging directory is
created. Failed runs retain diagnostics but publish no release evidence directory.
Kubeconfigs, runtime configuration and other cluster diagnostics are never copied
wholesale into release evidence. Raw controller logs remain local; the packaged
canary scan contains only counts and its explicit synthetic-canary scope. This
scan does not establish the absence of all possible secret material. The release
verifier binds `evidence/run_cluster.py` to the runner source digest, and staging
rejects source drift during measurement. `cuenv task reactorPackagingTest` verifies this
boundary using synthetic records; it does not establish fresh cluster acceptance.

`cuenv task reactorEvidenceLive`, with `APS_REACTOR_EVIDENCE_RELEASE` selecting an
absolute assembled release directory, checks production payload integrity, exact
production/fixture test-plan equality, every required core reactor case and the
three fixture checks through the actual evidence validator. It accepts no extra
case or check keys. This opt-in consumer grants no `VerifiedRelease`, does not
verify signatures or the remaining production checks, and does not separately
accept each supplemental `fixtureCases` entry.

## Fixture promotion verification

`cuenv task fixturePromotionLive` exercises the real release verifier against an
exact signed production candidate and a separately built fixture release. Set
`APS_FIXTURE_PRODUCTION_ARTIFACT`, `APS_FIXTURE_ARTIFACT`, and
`APS_FIXTURE_TRUST_POLICY` to absolute paths. Build the fixture with
`reakt build --source ... --mode fixture` and assemble it with `reakt release`.
Its coverage remains pending; the driver does not manufacture passing coverage.

The driver verifies the production signature, validates fixture native binary
and OCI integrity, binds every reread document to its manifest, and proves the
exact compiler projection and unchanged reactor contracts. It requires the
specific fixture-promotion rejection and rechecks both subjects. The fixture
rejection precedes signature verification, so this does not establish that the
fixture has a valid signature.

`tests/reakt/run_fixture_promotion.py` packages a fresh driver execution as a
single production evidence check. It accepts existing candidates and the fixture
compiler stage; it does not rebuild or modify them:

```sh
cuenv exec python3 tests/reakt/run_fixture_promotion.py \
  --artifact "$PWD/work/deployment-ceiling-899233f-subject/candidate" \
  --trust-policy "$PWD/work/deployment-ceiling-899233f-subject/trust.json" \
  --fixture-artifact "$PWD/work/fixture-promotion-0a3b58b/candidate" \
  --fixture-source "$PWD/work/fixture-promotion-build-0a3b58b.source" \
  --output "$PWD/work/fixture-promotion-evidence"
```

The signed production candidate's fixture IR and overlay must match the supplied
compiler stage byte-for-byte. The runner freezes both releases, their payloads,
trust policy, stage inputs, verifier source closure and executable across the
measurement. Only a successful exact rejection produces an evidence receipt.
The fixture identity is supplemental; the receipt belongs to the production
candidate. Namespaced source archives and the measurement accompany the receipt
for the normal `reakt merge-evidence` consumer. `cuenv task fixturePromotionTest`
checks orchestration contracts without substituting for the real verifier run.

The producer at commit `d8677cd` passed against the real Linux/arm64 fixture and
signed production candidate shown above. The receipt is in
`work/fixture-promotion-evidence-d8677cd/evidence/fixture-promotion-evidence.json`.
The standard merger accepted it with the existing receipts:
`work/production-eight-merged/summary.json` reports eight covered checks, no
missing reactor cases (43 covered), and `releaseVerified: false`. The remaining
checks are scope-matched benchmark, permission-changing upgrade, phase
interruption recovery, last API type cleanup, UID replacement rejection,
finalizer preservation, uncertain-writer refusal, and API-bound gap conformance.


## CNPG permission-changing upgrade

`tests/reakt/run_cnpg_demo.py` produces the `permission-changing-upgrade` check
from the exact signed initial/successor CNPG pair. It verifies rejection under
the initial policy before successor creation, unchanged registry and initial
writer identities, explicit policy expansion, and the actual staged, quiesced,
activated and committed successor lifecycle. The no-unapproved-writer assertion
is scoped to the owned, single-actor test cluster.

The producer freezes the candidate payloads, trust, operator manifest, tracked
source archive and executables. It publishes atomically only after successful
application checks, input revalidation, cluster cleanup and credential removal.
Retaining the demo cluster suppresses receipt publication. The receipt belongs
to the successor; the initial candidate is supplemental evidence.

The live producer at `fed891c` passed using the signed pair built from `5ed7b25`.
The receipt is
`work/cnpg-permission-evidence-fed891c/evidence/permission-upgrade-evidence.json`.
The normal `aps merge-evidence` consumer accepted it for the exact successor in
`work/cnpg-permission-merged-fed891c/summary.json`: one covered check, fifteen
missing checks, 43 missing reactor cases, and `releaseVerified: false`. This
CNPG payload differs from the earlier eight-check package; those receipts must
not be combined.


## Exact cleanup candidate pairs

The [CNPG cleanup predecessor](../../examples/cnpg/cleanup/README.md) provides
the removable Backup resource. The signed-pair driver accepts an explicit
purpose and validates it with the same compiled-plan relation used by the
platform. A replacement UID has a typed refusal; the driver reports it only
after authorized readback confirms the transition remains in Cleaning with
the exact original UID claim retained. These are producer prerequisites, not
live cleanup receipts.

The cleanup predecessor was built for Linux/arm64 at source revision `aa04c76`
and assembled as release
`sha256:c890f42c4624bba3aca59c1bbb1f04a3fb4cb9cb151de97540db1712e6a36a4f`.
Its native binary digest is
`sha256:27f0929c7f1f37b83ffad6aa51ff0127b2bb281769389386b8ddaa7318c98c47`.
The public signed-pair verifier accepted it with the unchanged CNPG successor
under joint trust digest
`sha256:0f1ede6a85f0fd14e1081cd3940898783b01f9dd20a61557109d884e24819ead`.
The temporary private signing key was deleted. This verifies signed candidate
integrity and the exact compiled cleanup relation; it does not qualify either
live cleanup scenario or full release acceptance.

The test-support pair marker requires an explicit purpose. Permission-expansion
qualification retains its strict capability addition and distinct approved policy
identities. Last-type cleanup qualification instead binds one selected resource
identity and one stable policy covering both runtime generations and cleanup.

A cleanup predecessor must equal the successor plus exactly that resource with
prior deletion policy `delete`. The validator recompiles the predecessor after
removing the selected resource and requires the exact successor plan; only new
source coordinates and provenance may differ. Surviving declarations, inputs,
reactions and derived behavior cannot change under this purpose. The successor
must have no capability for the removed API group/resource.

Before reservation, and when resolving the frozen successor session, qualification
requires exactly one canonical cleanup item with its trusted UID, predecessor
generation and exact named `get`/`delete` capabilities. Ordinary protected
predecessor, policy, inventory, writer-retirement and lifecycle checks still apply.
The permission-rejection operation cannot be used with the cleanup purpose.

This enables qualification of an exact signed cleanup pair; it does not itself
produce finalizer-preservation, UID-replacement or last-api-type-cleanup receipts.
The separate live scenarios passed at runner revision `2c2c56b` on kind through
OrbStack using the signed pair above:

- `work/cnpg-finalizer-first/measurement.json` records pending cleanup with the
  pinned UID and retained claim, followed by committed cleanup after removing
  only the test-owned finalizer.
- `work/cnpg-replacement-first/measurement.json` records the typed UID refusal
  with the original protected claim retained in Cleaning. The replacement's
  non-status content and finalizers remained unchanged.

Both runs preserved an unclaimed same-type Backup and verified that the Role
bound to the successor in the workload namespace granted no Backup access.
This Role check is not a cluster-wide effective-authorization proof. Both
runners removed their cluster and credentials, restored the previous inventories,
and reverified the frozen inputs and signed pair before publication.

The original measurement files remain unchanged. Their production receipts are
in `work/cnpg-finalizer-receipt` and `work/cnpg-replacement-receipt`.
The normal `aps merge-evidence` consumer accepted both alongside the existing
permission-upgrade receipt in `work/cnpg-cleanup-merged`. This covers four of
sixteen checks for the same successor; twelve checks and 43 reactor cases remain
missing, and `releaseVerified` is false. Negative consumer checks rejected altered
binary and runner digests without publishing output.
Independent read-only review passed the adapter and its ten tests, rechecked
unchanged measurement bytes and runner hashes, and confirmed that both packaged
closures exactly match the merged evidence. That review used archived records;
it did not rerun the cluster experiments.

Packaging these archived measurements must preserve the original measurement
bytes and measured producer source. The adapter must check an explicitly supplied
measurement digest, the frozen candidate and producer files, and the scenario's
lifecycle observations before deriving release assertions. The finalizer run can
support `finalizer-preservation` and `last-api-type-cleanup`; the replacement run
can support `uid-replacement-rejection`. Neither can satisfy reactor fixture cases.
The source archive explains the guarded Kubernetes readbacks behind those
observations; packaging does not repeat them or independently authenticate their
execution. Receipt filenames must distinguish both scenarios from the existing
permission-upgrade receipt so the normal merger can retain each source closure.

The bounded archival adapter is `tests/reakt/package_cnpg_cleanup.py`:

```sh
cuenv exec -- python3 tests/reakt/package_cnpg_cleanup.py \
  --measurement work/cnpg-finalizer-first \
  --measurement-sha256 sha256:d8d9898e67f7e5ea9acc45d83e50521f7526f2353138199b01df9ad97be96114 \
  --initial-artifact work/cnpg-cleanup-pair-aa04c76/initial \
  --successor-artifact work/cnpg-pair-5ed7b25/successor \
  --trust-policy work/cnpg-cleanup-pair-aa04c76/trust.json \
  --output work/cnpg-finalizer-receipt
```

The destination must not exist. Publication creates it exclusively and writes
the receipt last; a directory without a valid receipt is incomplete and must not
be selected for merging. `cuenv task cnpgCleanupEvidenceTest` checks incomplete
observations, changed bindings and source archives, and output protection.
