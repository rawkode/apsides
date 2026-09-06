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
