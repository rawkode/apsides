# Protected Kubernetes platform state

`platform` uses an authenticated `kube::Client` supplied by the trusted deployment environment. It never loads a project kubeconfig, runs a project hook, or accepts an artifact's policy as platform authority. Initial platform bootstrap installs the control objects and permissions; routine execution does not require a permanent Apsides administrator.

All JSON uses `schemaVersion: "apsides.dev/platform/v1"`, camelCase fields, strict unknown-field rejection, and a 512 KiB serialized state limit. Every ConfigMap stores exactly one data entry, `state.json`. Kubernetes metadata UID and resourceVersion are read separately and retained across conditional PUTs. Missing, corrupt, deleting or oversized records block. No expiry grants a stale claim or transition to another executor.

| Control object in the separate control namespace | Writer | Reader / schema |
| --- | --- | --- |
| `apsides-policy-{domain}` | platform administration | runtime and deployer; `PolicyState { runtimeCeiling, cleanupCeiling, imagePullSecrets }` |
| `apsides-trust-{domain}` | platform administration | deployer only; release verifier's policy JSON |
| `apsides-registry-authority.{controlNamespace}.{workloadNamespace}` in `apsides-platform` | platform administration and explicit recovery only | deployer exact GET only; scope and admitted registry UID |
| `apsides-registry-{workloadNamespace}` | authorized deployment jobs | deployer only; `NamespaceRegistry` |
| `apsides-activation-{domain}` | authorized deployment job | runtime; `ActivationState` |
| `apsides-inventory-{domain}` | runtime updates a preprovisioned object | runtime and deployer; `RuntimeInventory` |
| Secret `apsides-rollout-key-{domain}` | authorized deployment job | runtime only when sensitive inputs require it; exactly `key` (32 bytes) and `keyId` (UTF-8) |

The namespace registry contains an immutable claim reference, one registry-wide monotonic allocation counter, a committed active allocation and writer checkpoint per domain, and compact transition records. Each transition binds frozen planning inputs, review references, capacity admission and issued-executor identity. The selected plan is reconstructed from the freshly verified release. Reservation compares the exact supplied snapshot and independently loaded ceilings, then atomically reserves resource identities, generation identity and transition ownership in one ConfigMap replacement. Kubernetes UID/resourceVersion predicates reject concurrent deployers. Allocation authority is never inferred from active claims. Counters are canonical decimal strings; snapshots and allocations bind the actual registry UID.

`load_trust_policy` returns the protected policy JSON plus Kubernetes UID/resourceVersion for the separate established signature verifier. `load_policy` similarly returns the typed ceilings. Hashes identify these records; their writers' platform-controlled RBAC establishes trust. The release verifier must reject fixture artifacts and bind approved provenance, compiled plan, controller image and build manifest before deployment proceeds.

`RuntimeBinding` contains domainId, clusterBinding, generationId, buildDigest, irDigest, policyDigest, capabilityDigest, serviceAccountName and controllerImage. The image is a digest-pinned OCI reference. The policy digest is `compiler_core::digest(PolicyState)` and capability digest is `compiler_core::digest(compiled.capabilities)`. These are canonical typed digests, not hashes of arbitrary JSON formatting. The binding comes from a trusted Deployment, but is not by itself proof of which credential a process is using.

`ActivationState` contains binding, status (`inactive`, `active`, `draining`), approvedClaims and rolloutKeyId. Each `ApprovedClaim` contains the existing deployment `ProtectedClaim`, deletionPolicy and allowCreate. Claim scope must match exactly every IR resource, with the current domain/generation and Active state. A UID authorizes recognition of that specific existing object. A missing UID never authorizes adoption based on field-manager metadata. `allowCreate` permits an absent-only creation when actual compiled capabilities include create; it does not approve a pre-existing object.

`RuntimeAuthority::verify` validates the trusted compiled plan, rejects test-only IR, reads bound Active activation and the independently protected policy, recomputes all identities and validates the full claim set. Runtime must recheck before each mutation. This is cooperative validation with a possible read/write race, not hard fencing of malicious processes or already accepted requests. `read_activation` exposes Draining status for shutdown. `publish_drained_inventory` accepts only freshly read Draining state and `drained: true`; that acknowledgement is diagnostic, never proof that Kubernetes completed previously submitted writes.

Runtime inventory includes the binding, activationUid, activationResourceVersion, observed resource identities/UIDs, bounded machine-readable conditions and drained. It cannot enlarge scope or authorize deletion. Publication rereads activation and conditionally replaces the existing inventory, preserving UID and resourceVersion. Runtime needs GET+UPDATE only, never CREATE. Other runtime support requests are named GET on activation and policy, and named GET on the key Secret only when required. Runtime must have no registry, trust-policy, activation, key, RBAC or controller-management write authority. There are no claim TTLs.

`request_deployment_credential` uses the standard Kubernetes TokenRequest API for a preprovisioned ServiceAccount and live UID-bound Secret. It validates both live identities, requests one explicit audience and 600–900 seconds, and rejects a returned expiration exceeding the request. Token bytes use a non-serializable secret type. This is credential minting after the caller has authenticated; it does not translate arbitrary OIDC claims into Kubernetes identity or replace the platform's issuer/audience/subject/workflow policy.

## Process and request termination gate

Kubelet-reported termination of all containers in retained Pod UIDs can establish process termination in a qualified environment. It does not establish completion of in-flight API writes. A force-deleted or disappeared Pod, an unreachable node, a cooperative drained flag, expired timeout, or deleted RoleBinding is insufficient on its own.

The qualified credential-injecting API proxy is the supported request-drain boundary. Runtime must not receive a Kubernetes token. A proxy-only token volume and a proxy-admin Unix socket inaccessible to runtime avoid the shared Pod-network admin exposure. Graceful Envoy listener draining initially continues accepting connections; zero active upstream requests also does not distinguish completed responses from timed-out/reset requests whose Kubernetes outcome is unknown. Qualification must establish that admission of new requests has stopped and that every forwarded mutation has a terminal upstream response, with continuous protected evidence and no ambiguous outcomes. Unsupported environments remain blocked; this crate does not manufacture a proof from caller-provided digest strings.

## Private controller images

Platform bootstrap may set `imagePullSecrets` in `apsides-policy-{domain}` to at
most sixteen unique Kubernetes Secret names, for example
`"imagePullSecrets": ["controller-registry"]`. These references apply to the
controller and its proxy Pod in the control namespace. The platform provisions
the registry Secrets there separately; their contents are not deployment inputs,
inventory records, mounted runtime files or output fields. Kubelet performs the
image authentication. This grants no additional runtime Secret permissions.

An omitted or empty list uses the cluster's existing image authentication, such
as node credential providers, or public images. Controller references still use
immutable image digests. References are part of the protected policy digest and
staged Pod template integrity checks. Writer topology checks reject live Pods
whose references differ from their Deployment template. Changing policy references
during a transition causes
the existing policy-drift checks to reject resume. Credential rotation under an
unchanged name remains a platform operation and does not bind Secret contents or
resource versions into Apsides state.

Manifest tests establish reference propagation, not successful registry access.
Private-registry qualification requires a fresh node pull of the exact image
digest and a failure case with missing or invalid credentials. Preloaded demo
images do not establish that evidence.

## Initial installation executor

Enable `platform`'s `deployer` feature for `stage_controller(transition_id,
executor_id, &IntegrityVerifiedCandidate)` and `activate_initial(transition_id, executor_id)`.
The former reloads independent trust and ceilings, checks the non-deserializable
verified release against the reserved plan, stages exact generation RBAC, an
immutable runtime configuration, inactive activation, preprovisioned inventory,
and a zero-replica Deployment. Every successful object creation is followed by
registry UID/content persistence. An object created before a failed UID-persistence
CAS remains blocked for explicit platform recovery; matching names or annotations
alone never authorize adoption.

The runtime receives no source checkout: its configuration names only the compiled
IR identity, immutable native image, activation bindings, and bounded runtime
settings. Startup uses `APSIDES_RUNTIME_CONFIG_PATH=/etc/apsides/config.json`, with no
extra executable arguments. Initial activation persists the initial quiescence
state, active claims and history, then the protected active marker before starting
the Deployment. Resume uses the same recorded executor, UID and object resource
versions. `Committed` records deployment transition completion, not workload
readiness or an end-to-end behavioral pass.

Initial installations record
`requestDrainQualification: "unqualifiedDirectApi"`. They use the runtime's
`inCluster` transport under the admitted API-bound enforcement gaps. This marker
cannot authorize a later automatic upgrade. An upgrade requires qualified proxy
transport and complete protected request-ledger evidence.

## Termination and proxy qualification

`observe_writer_termination` reads the known Deployment, the complete protected Pod
set, and Node identities. It requires observed zero replicas, preserved Pod
finalizers, known unchanged Ready Nodes, terminal Pod phases and terminal statuses
with concrete container IDs for every normal, init and ephemeral container. A
missing Pod, incomplete observed generation, replaced Node, unknown container or
changed Deployment fails. Its result is a process observation, never an activation
authority or API-drain proof.

The Envoy canary in `crates/platform/tests/envoy/canary.py` exercises the
standard all-request health-check filter before credential injection and routing.
Admin `/healthcheck/fail` freezes admission independently of controller behavior.
An isolated curl helper accesses the UNIX admin socket because the pinned Envoy
image contains neither curl nor Python. Neither the socket nor Kubernetes bearer
credential may be mounted into the runtime Pod. The proxy and admin helper are a
separate trusted Pod. The canary carries only an invented fixture token.

Idle counters alone are insufficient. Accepted requests must have a complete
access-log entry, exactly one upstream attempt, no response flags, and
`via_upstream` completion with an explicitly accepted status. Upstream 408/5xx,
timeouts, resets, log gaps, proxy restarts and identity changes block drain.
The all-request health-check filter marks requests as health checks, which causes
Envoy router dynamic status counters to skip them; explicit access logging and
request counters are required. Canary output is environment qualification evidence,
not a substitute for a durable, protected per-generation ledger and live process
termination/revocation checks.

Primary API references: [Envoy health-check filter](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/health_check/v3/health_check.proto),
[credential injection](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/credential_injector_filter),
and [access logging](https://www.envoyproxy.io/docs/envoy/latest/configuration/observability/access_log/usage).

## Isolated generation staging

`stage_proxy_controller(transition_id, executor_id, &IntegrityVerifiedCandidate)` stages the
fixed Envoy proxy before the zero-replica native controller. A protected
`apsides-proxy-policy-{domain}` ConfigMap supplies `ProxyPolicy`: schema, domain and
cluster bindings; the fixed Envoy/admin image digests; the digest of
`envoy_configuration("qualified-template")`; independent environment qualification
and node image digests; certificate lifetime (one day to one year); and the exact
16 MiB ledger limit. Node image identity is an explicit platform bootstrap
qualification: Kubernetes Node status does not establish a container image digest.

The platform uses rcgen/ring to issue a fresh CA and server/client certificates for
each generation. The CA private key is discarded. Immutable Secret item projections
mount only server keys into Envoy and client keys into the runtime; the runtime uses
an inert service account with no RoleBindings and no Kubernetes token mount.
Envoy alone receives the rotating projected Kubernetes token. Standard Envoy SDS
watches the token file's directory; the canary tests an atomic token rename.

The recorded proxy identity binds a ready, unrestarted Pod through its ReplicaSet
owner to the staged Deployment UID and API-defaulted template, plus its Node UID,
container IDs, immutable configuration UID/content and protected policy digest.
Initial activation rechecks those bindings. Both Deployments use `Recreate` and
indefinite not-ready/unreachable NoExecute tolerations. Forced deletion and Node
identity changes do not constitute writer termination evidence.

Writes use a separate `write_api` Envoy cluster and write-only ledger. Read/watch
cancellation does not poison the write ledger. The all-request active counter must
still reach zero before accepting a frozen ledger, ensuring no request remains
between admission and routing. The 16 MiB complete ledger is a finite generation
operation budget; overflow, truncation or lost records blocks automatic upgrade.
No compaction or indefinite lifetime claim is made. Proxy staging and the local
Envoy canary do not by themselves prove live Kubernetes transport, complete writer
termination, obsolete credential retirement or upgrade execution.

## Upgrade writer and recovery boundary

For an isolated generation, the separate proxy is the sole credential-bearing
Kubernetes API writer. Its original Pod UID, container IDs and zero-restart epoch
are captured before runtime activation. `quiesce_previous` freezes admission,
stops the currently observed tokenless runtime Pods, verifies the complete write
ledger, terminates that exact proxy epoch and retires its recorded RoleBindings
and service account with UID/resourceVersion preconditions. Every successful step
is recorded in protected registry CAS state. Resumption after a proxy restart,
missing Pod, incomplete ledger or uncertain Node cannot reuse an old drain result.
The terminal Envoy and admin container IDs must equal the frozen ledger's epoch.

The tokenless runtime stop set is operational cleanup, not proof that every
historical tokenless runtime process has died. A surviving copied client can only
present the old generation's mTLS certificate to the frozen old proxy; the next
proxy has a fresh CA. This establishes termination of the generation's configured
API writer. It does not confine credentials acquired through separately admitted
indirect workload privileges; those remain an explicit API-bound policy risk.
The canary verifies continued client writes are rejected after freeze, and the
standard OpenSSL interoperability test rejects an old client certificate under a
new generation CA. A live Kubernetes mTLS lifecycle test remains separate evidence.

`activate_upgrade` independently rechecks protected policy, next proxy identity,
old terminal epoch and retired credentials, then transfers approved claims,
publishes activation and resets only the generation-bound inventory before starting
the next controller. `commit_upgrade` invokes actual cleanup internally and uses
one fresh registry CAS to release only completed claims. Caller-supplied cleanup
receipts cannot commit a transition.

The optional `test-support` build exposes `stage_test_candidate` only for separately
verified candidate authorization. It verifies the actual `kube-system` Namespace
UID plus a UID-pinned `apsides-test-support` ConfigMap of schema
`apsides.dev/disposable-test-cluster/v1`, with exact cluster/control/workload scope.
The cluster binding must begin `apsides-disposable-`, and namespaces `apsides-test-`.
This acceptance capability never constructs `VerifiedRelease` and is absent from
production CLI builds. Ordinary `aps deploy` independently verifies artifact trust and platform
authorization. Full qualification evidence is checked by `aps verify-release`.

Every activation/resume checks the staged service account and exact derived Roles
and RoleBindings, including their recorded UIDs and current semantic contents.
It also rereads immutable runtime configuration and generation TLS identities.
These checks establish that Apsides' own grants have not drifted. Kubernetes RBAC
is additive: independently administered bindings can grant the same service account
additional authority. Preventing that is a platform authorization/admission trust
assumption; this adapter does not claim to prove the complete effective ceiling
by inspecting only its own objects. Actual short-lived credential tests establish
permissions in the tested cluster at the test time.

`Error::Pending` reports ordinary readiness and observed scale-down convergence.
Invalid quiescence identity, terminal epoch or request-ledger evidence produces a
protected `Blocked` transition. A timer never converts pending or blocked evidence
into permission to activate.
