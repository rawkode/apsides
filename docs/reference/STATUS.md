# Apsides implementation status

Apsides targets specification 0.2. Existing `apsides.dev` identifiers are its active wire names.
Apsides is **not ready for production use**. Unit tests, the reactor cases of
`aps test --cluster` and the acceptance suite on disposable kind clusters
establish narrower properties.

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
  Unit tests in `crates/cli/src/assets.rs` check that an added, changed or
  removed asset fails verification, including the compiler, SDK, lockfile, a
  linked dependency, the runtime crate and the native builder, and that the
  restored tree passes again. `cuenv task packageCliTest` covers the packaging
  script. No test runs a relocated bundle with checkout reads denied.
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
  Server-Side Apply. It cannot expand its approved scope.
- Declared handlers run as bundled TypeScript, one fresh V8 isolate per
  invocation, in the `engine` crate embedded by `runtime` (dependency chain
  `cli` → `test-cluster` → `runtime` → `engine`). At most one invocation
  dispatches per reconcile cycle, at global concurrency 1; `apsides.fetch` is
  restricted to declared HTTPS egress hosts resolved through a dedicated
  DNS resolver built from `/etc/resolv.conf` (never glibc's own
  `getaddrinfo`, and only when some declared handler has egress hosts);
  handler output writes are buffered in memory for the invocation and
  committed as one server-side apply per declared output, under field
  manager `apsides-handler-<domainId>`, through the same authority/claim
  re-check reconciler mutations use.
- Protected platform policy can supply registry Secret references for controller
  and proxy image pulls, without granting runtime Secret access. References bind
  to the policy and staged Pod templates; credentials remain platform-owned.
  The acceptance suite's runtime credential scenario deploys under a policy
  naming a pull secret and checks that the staged controller and proxy
  Deployments and the proxy Pod carry exactly that reference. Registry
  authentication itself is the kubelet's and is not tested.
- Runtime diagnostics retain completed-cycle blocked counts and specific reasons
  throughout reconciliation backoff. `/status` includes up to 256 `conditions`
  containing only `resourceId` and `reasonCode`, plus a `fieldOrVersionConflicts`
  count. A Kubernetes 409 is not classified more narrowly as an SSA ownership
  conflict. Operational failures replace those with a pending cache state and
  their own reason, clearing the per-target list and conflict count. `/readyz` stays unavailable while
  degraded; a clean cycle restores readiness. These probes describe the
  controller, not application workload health.
- The field-manager conflict cases `aps test --cluster` generates check that
  a competing manager's field is reported as a conflict and not forcibly taken
  over. They check controller diagnostics for the fixture, not workload health.

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
  linux/arm64, and the `reactor` job of the checks workflow runs them on every
  change. Apsides itself is accepted by the Rust
  [acceptance suite](/reference/acceptance/), which runs in CI on the examples.

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

The [acceptance suite](/reference/acceptance/) runs in CI and deploys signed
releases of the CNPG examples with the public `aps deploy` on single-node kind
clusters with a synthetic signer. Its reference lists each check and the §28
row it answers. In summary:

- the upgrade scenario rejects a successor above the ceiling before any
  successor object exists and with the registry unchanged, commits it once the
  ceiling is widened, retires the old identity, preserves the database, rolls
  the API out on ConfigMap and Secret changes but not on equal data, and finds
  no Secret value, rollout key or token in output, logs or the inventory
  database;
- the runtime credential scenario reads the proxy's projected token into memory,
  has `TokenReview` bind it to the proxy Pod and runtime `ServiceAccount`, and
  shows that every declared capability succeeds, each disclosed RBAC gap behaves
  as disclosed, out-of-scope requests are denied, `SubjectAccessReview` agrees,
  and a synthetic Secret-key change rolls the API out and back. It measures
  direct Kubernetes API access with the runtime credential, not proxy-path
  enforcement;
- the collision scenario refuses a second domain's claim with no state change,
  and shows that a live preview approved by the deployment identity changes no
  Kubernetes state before the reservation and is carried in the committed
  report;
- the phase, uncertain-writer and cleanup scenarios stop `aps deploy` after each
  stored phase and resume it, block activation after the predecessor's
  termination evidence is lost, run last-type cleanup through an unresolved
  finalizer and refuse a replacement UID. They stop runs with admission-policy
  holds and SIGKILL rather than a hook in the product. The acceptance reference
  lists exactly which interruption points each scenario proves.

Reproducibility is checked twice: two `aps compile` runs of
`examples/web-application` must give identical trees in the CLI tests on every
change, and two cache-disabled `aps build` runs must give identical files,
including the binary and OCI archive, in the nightly workflow. Both pass. The
packaging writes the final image layer and config itself, with every timestamp
set to the build profile's `sourceDateEpoch`, so the OCI archive and image
digest do not depend on the image store or on when the build ran. The native
check was last run on Docker's classic (overlay2) image store.

`cuenv task authorCompilerTest` checks the declared-input and SES capability
boundary on every CI run. A program importing a declared module compiles. An
undeclared import fails at typecheck with a diagnostic that names the module and
the importing source line. A dynamic import fails while bundling. Clock,
random, environment, filesystem read, network, WebAssembly, stream,
constructor-escape and prototype-mutation attempts each fail during evaluation
with their own diagnostic, raised inside the program's default export. These
tests do not check operating-system syscall or network isolation.

Automated suites cover strict schema and canonical replay, claim and metadata
store immutability, registry concurrency, capacity accounting, review privacy,
issued-client binding, preview request shape, UID-safe cleanup, finalizer retry,
audit retention, and release integrity/signature rejection. Strict Clippy checks
apply the repository's production-source error-handling rules.

The local SQLite durability tests for inventory, claim, review and audit
records are described in [resource inventory](/reference/inventory/#current-limits).

## Remaining production work

- The GitHub Actions checks workflow runs the cuenv build, authoring, editor,
  Rust, packaging-script and documentation tasks, the reactor cases and the
  acceptance suite with read-only repository permissions; the nightly workflow
  runs the native double build. Release signing and external
  OIDC-to-Kubernetes identity configuration remain separate unfinished work.
- The runtime watches only observed inputs, matching compiled permissions;
  managed inputs retain periodic reconciliation. Database credential rotation
  is not tested.
- Add a release workflow for the `aps` distribution that signs only after the
  acceptance suite passes.
- Keep uncertain stalled-job takeover blocked. A lease, expired token or changed
  executor label alone does not establish that the previous executor cannot
  proceed. The spec includes the trusted deployment job in its trusted computing
  base and forbids requiring a new broadly privileged coordinator.
- The SQLite metadata store is tested on the local filesystem only. Persistent
  volumes, node loss, power loss and remote storage are not tested.
- Complete production API authorization and admission qualification for the
  supported Kubernetes versions, including the declared preview permissions and
  request-drain profile.
- Complete the performance gate. No scoped performance comparison runs; the
  Python sampling that measured earlier candidates has been removed.
- Define operator recovery for incompatible or corrupt protected state. Current
  schemas fail closed and do not infer missing authority or perform automatic
  conversion.

Specification 0.2 remains the normative target. The contract pages describe the
implemented subset and explicitly identify boundaries that still require live
qualification.
