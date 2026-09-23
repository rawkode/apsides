# Acceptance suite

The acceptance suite (`crates/acceptance`) accepts the Apsides implementation
against the example programs, as [specification §28](/reference/spec-v0-2/#28-initial-release-acceptance-criteria)
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
  the only Kubernetes version `aps deploy` accepts.

```sh
cuenv task acceptancePrepare   # build, release and sign both CNPG releases
cuenv task acceptanceTest      # run every scenario, one kind cluster each
cuenv task acceptanceCheck     # Clippy for the suite
```

`acceptancePrepare` generates the CloudNativePG types, builds releases of
`examples/cnpg/initial` and `examples/cnpg` with `aps build` and `aps release`,
signs each release's provenance with an OpenSSH key generated for that run
(the private key is deleted once both are signed), and writes a trust policy
approving only that key, the releases' compiler digests and `/usr/bin/ssh-keygen`.
It verifies both releases with `aps verify-release`, fetches the CloudNativePG
1.30.0 operator manifest and accepts it only with its pinned SHA-256. It writes
`work/acceptance/inputs.json`, which `acceptanceTest` passes to the scenarios as
`APSIDES_ACCEPTANCE_INPUTS`. A scenario fails, and never skips, when that file is
missing.

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

| Scenario (`tests/kind.rs`) | Check | §28 row |
| --- | --- | --- |
| `cnpg_upgrade_through_the_public_cli` | (a) under a ceiling allowing only the initial release, `aps deploy` of it commits | Upgrade to a build with different permissions |
| | (b) `aps deploy` of the successor fails with the ceiling diagnostic for the capability it adds; no successor or next-generation object exists, the registry's UID and resourceVersion, the reserved sequence and the running initial writer are unchanged | Add a capability above the ceiling |
| | (c) after the platform widens the ceiling, `aps deploy` of the successor commits a new transition and generation | Upgrade to a build with different permissions |
| | (d) every RoleBinding and the `ServiceAccount` the old generation recorded for retirement are gone, no RoleBinding names the old identity, and the new `ServiceAccount` has another name and UID | Upgrade to a build with different permissions |
| | (e) a row written before the upgrade reads back afterwards; the CNPG Cluster and every volume claim keep their UIDs | (example data preservation) |
| | (f) a ConfigMap change rolls `PostgREST` out to a new revision, an equal-data write causes no rollout for 30 s, and a Secret change rolls it out and back under the same rollout key | Reconcile unchanged ConfigMap/Secret input repeatedly |
| | (g) no Secret value, rollout key or issuer token appears in `aps` output, the inventory database files or any control-namespace container log | Inspect artifacts, logs, metrics, and inventory |

The reactor rows (generated tests, fixture images, offline execution,
reaction-mutation checks) are covered by `aps test --cluster` in the `reactor`
CI job. The remaining rows (reproducibility, undeclared input, credential
scope, field-patch coverage, downtime recovery, watch expiry, input denial,
out-of-band edits, field-manager conflicts, claim collision, phase
interruption, last-type cleanup, replacement UIDs, finalizers, uncertain
writer termination and fixture promotion) are still qualified by the Python
harnesses in `tests/` until each is ported here.

## Limits

- One single-node kind cluster per scenario, Kubernetes v1.34.3 only.
- The release signer is synthetic: an ephemeral OpenSSH key trusted only by the
  scenario's own policy. The GitHub Actions attestation path of the trust policy
  is not exercised.
- The issuer credential is a TokenRequest token minted with the cluster's admin
  kubeconfig, not a Pod-bound token or an external OIDC identity.
- The Envoy environment canary (`crates/platform/tests/envoy/canary.py`) is not
  run. The proxy policy's environment qualification digest names that fact
  rather than a canary result.
- The images the platform and the examples pin by digest (Envoy, curl, the CNPG
  operator, PostgreSQL and PostgREST) are pulled into the kind node from their
  registries during the run, so the run needs registry access. A classic
  (overlay2) Docker image store rewrites manifests on `docker image save`, so a
  host-side copy could not stand in for the pinned digests.
- The platform permissions are the fixed grants the scenario installs; they
  show the deployment works with those grants, not that they are minimal.
- The canary scan covers `aps` output, the inventory database and controller
  logs. Metrics endpoints are not scraped.
