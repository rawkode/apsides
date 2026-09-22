# Optional OCI release retrieval

`release::fetch_release` accepts an immutable `ReleaseReference` and a trusted CI `OrasTool` path/digest. The reference binds both the OCI manifest digest and the digest of `release.json`; these are different identities. Tags, URL schemes, embedded credentials and malformed references reject before retrieval.

The resolver delegates registry transport to [ORAS pull](https://oras.land/docs/commands/oras_pull/), with a fresh private output directory, fixed arguments, no interactive input and a five-minute deadline. It explicitly sets `--plain-http=false`: ORAS otherwise selects HTTP for localhost. It does not enable insecure TLS or path traversal. An optional trusted CI CA file supports private registry certificate chains; it is passed explicitly with `--ca-file`. Registry authentication uses ORAS's normal trusted CI configuration. The executable's bytes must match the configured digest. As with other CI tools, the executable and its ancestors must remain under trusted administration during execution; a digest check is not protection against a concurrent privileged replacement.

Transport success does not authorize a release. `fetch_release` checks the downloaded `release.json` against the reference's protected digest and, once they match, returns `FetchedRelease`, which owns the temporary directory. This establishes content identity only; it does not check signatures or policy. The CLI separately calls `release::verify_candidate` against that path with an independently supplied `TrustPolicy`, covering payload integrity and signature policy, before treating the release as deployable. Dropping `FetchedRelease` removes the downloaded directory; persistent retention belongs to the external artifact registry. Failed retrieval returns no fetched handle. Registry diagnostic output is suppressed rather than included in errors.

The deployment CLI supports this retrieval boundary and applies the independently supplied trust policy to the requested release. Artifact publication for a complete signed release, retention policy, disk quota enforcement and a fresh-CI upgrade remain unverified.

## Deploy from an immutable reference

Supply `--artifact-reference FILE`, or set `APSIDES_RELEASE_REFERENCE` to that file and run `aps deploy`. The file is strict JSON with only `oci` and `releaseDigest` fields. `oci` must be a repository followed by `@sha256:` and the OCI manifest digest; `releaseDigest` must be the SHA-256 digest of the exact `release.json` bytes. Both hashes must come from the approved release publication. Do not substitute the controller image digest for either value.

CI configures `APSIDES_ORAS` as an absolute executable path and `APSIDES_ORAS_SHA256` as its `sha256:` digest. `APSIDES_REGISTRY_CA` optionally names an absolute trusted CA file. Registry credentials stay in ORAS's normal CI authentication configuration. Inventory settings remain required. Explicit local `--artifact` and a configured remote reference are mutually exclusive.

The CLI retains the temporary download throughout the operation, discovers cluster policy from the release's declared scope using the existing bootstrap flow, and verifies the release under that protected policy. It compares the approved digest with the requested release before minting deployer credentials. The fetched-content type confers no deployment authority. The temporary files are removed when the operation ends; immutable registry retention must outlive CI and pod replacements.

This supports fetching the requested deployment artifact. A complete signed-release deployment from a fresh CI environment remains unverified.

OCI retrieval is optional distribution for a CI-supplied release. Normal upgrade and prune recovery must not retrieve a historical artifact. No OCI recovery location is written to protected transitions.
