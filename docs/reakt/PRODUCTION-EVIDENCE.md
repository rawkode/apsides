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
cuenv exec -- target/debug/reakt merge-evidence \
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
  --cli "$PWD/target/debug/reakt" \
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
  --cli "$PWD/target/debug/reakt" \
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

The source build at commit `0a3b58b` passed this check with a real Linux/arm64
fixture image and the archived production candidate. Its local result is
`work/fixture-promotion-0a3b58b/result.json`. This is a measured driver result,
not a packaged `ProductionEvidence` receipt. Source-closure packaging and merge
verification remain necessary before counting `fixture-promotion-rejection`
in production coverage, which remains seven of sixteen checks.

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
