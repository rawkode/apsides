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

The build profile, pinned tools and local image prerequisites remain the normal
build requirements. A mismatch or build failure is a failed qualification; it is
not permission to discard differing payload fields. Matching builds establish
payload reproducibility for those measured runs. Declared-input isolation remains
a separate mandatory release check.

`cuenv task reproducibilityTest` exercises runner failure handling with synthetic
build processes. `cuenv task releaseTest` exercises signature and evidence
verification. Neither command establishes production build or cluster acceptance.
