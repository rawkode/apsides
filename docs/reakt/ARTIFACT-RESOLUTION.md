# Optional OCI release retrieval

`release::resolve_release` accepts an immutable `ReleaseReference`, a trusted CI `OrasTool` path/digest, and an independently supplied `TrustPolicy`. The reference binds both the OCI manifest digest and the digest of `release.json`; these are different identities. Tags, URL schemes, embedded credentials and malformed references reject before retrieval.

The resolver delegates registry transport to [ORAS pull](https://oras.land/docs/commands/oras_pull/), with a fresh private output directory, fixed arguments, no interactive input and a five-minute deadline. It explicitly sets `--plain-http=false`: ORAS otherwise selects HTTP for localhost. It does not enable insecure TLS or path traversal. An optional trusted CI CA file supports private registry certificate chains; it is passed explicitly with `--ca-file`. Registry authentication uses ORAS's normal trusted CI configuration. The executable's bytes must match the configured digest. As with other CI tools, the executable and its ancestors must remain under trusted administration during execution; a digest check is not protection against a concurrent privileged replacement.

Transport success does not authorize a release. The resolver checks the protected release digest and invokes the existing complete release verifier, including signature policy and acceptance evidence. Only then does it return `ResolvedRelease`, which owns the temporary directory and verified release. Dropping it removes the downloaded directory; persistent retention belongs to the external artifact registry. Failed resolution returns no verified handle. Registry diagnostic output is suppressed rather than included in errors.

The deployment CLI now supports this retrieval boundary; metadata-only upgrade recovery is not yet migrated. It applies the independently supplied trust policy to the requested release. Artifact publication for a complete signed release, retention policy, disk quota enforcement and a fresh-CI upgrade remain unverified. Unit tests use a controlled transport executable. The separate opt-in transport test uses real ORAS and a disposable TLS registry, as described below.

## Real transport check

The opt-in `real_oras_transport_reaches_release_verification` test requires a disposable registry with a self-signed certificate and an OCI artifact containing exactly `{}` as `release.json`. Supply `REAKT_TEST_ORAS` (absolute executable path), `REAKT_TEST_REGISTRY_CA` (absolute trusted CA file) and `REAKT_TEST_OCI_REFERENCE` (immutable reference), then run:

```sh
cargo test -p release --lib real_oras_transport_reaches_release_verification -- --ignored
```

It checks that transport reaches release-schema verification, rejects a different protected release digest, rejects a nonexistent OCI digest, and rejects the registry when the CA is omitted. The deliberately invalid payload must never yield a verified release. This proves transport and rejection behavior, not successful production release promotion or deployment.

## Deploy from an immutable reference

Supply `--artifact-reference FILE`, or set `REAKT_RELEASE_REFERENCE` to that file and run `reakt deploy`. The file is strict JSON with only `oci` and `releaseDigest` fields. `oci` must be a repository followed by `@sha256:` and the OCI manifest digest; `releaseDigest` must be the SHA-256 digest of the exact `release.json` bytes. Both hashes must come from the approved release publication. Do not substitute the controller image digest for either value.

CI configures `REAKT_ORAS` as an absolute executable path and `REAKT_ORAS_SHA256` as its `sha256:` digest. `REAKT_REGISTRY_CA` optionally names an absolute trusted CA file. Registry credentials stay in ORAS's normal CI authentication configuration. Inventory settings remain required. Explicit local `--artifact` and a configured remote reference are mutually exclusive.

The CLI retains the temporary download throughout the operation, discovers cluster policy from the release's declared scope using the existing bootstrap flow, and verifies the release under that protected policy. It compares the approved digest with the requested release before minting deployer credentials. The fetched-content type confers no deployment authority. The temporary files are removed when the operation ends; immutable registry retention must outlive CI and pod replacements.

This supports fetching the requested deployment artifact. A complete signed-release deployment from a fresh CI environment remains unverified. Metadata-only predecessor recovery is a separate executor migration.

## Signed verifier fixture recovery

The separate opt-in `signed_oci_release_resolves_after_source_workspace_is_removed` test publishes the existing signed verifier fixture through real ORAS to an explicitly configured disposable TLS registry. It uploads only manifest-listed files plus the release manifest, provenance and signature; signing keys are excluded. It deletes the source directory, retrieves and verifies the artifact, checks automatic download cleanup, and rejects an unapproved signer policy against the same remote artifact.

Configure `REAKT_TEST_ORAS`, `REAKT_TEST_REGISTRY_CA` and `REAKT_TEST_OCI_REPOSITORY` (an isolated writable test repository), then run:

```sh
cargo test -p release --test signatures signed_oci_release_resolves_after_source_workspace_is_removed -- --ignored
```

This test passed with ORAS 1.3.4 and the pinned disposable Distribution image used for the transport check. Its binary and acceptance documents are verifier fixtures, not executed production-controller evidence. It proves signed-artifact retrieval without the source workspace, not live deployment or production promotion readiness. The registry was removed after verification.

OCI retrieval is optional distribution for a CI-supplied release. Normal upgrade and prune recovery must not retrieve a historical artifact. No OCI recovery location is written to protected transitions.
