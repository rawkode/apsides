# Apsides IR contract

This is the current implementation boundary for specification 0.3. It is not a claim of production readiness. Unknown fields and unsupported operations are errors.

`schemaVersion` is `apsides.dev/ir/v2`. JSON uses camelCase. Every declaration has `source: {file: string, line: positive integer, column: positive integer}`. Resource and input IDs are unique within their category. A reaction references input IDs and a managed resource ID.

```json
{
  "schemaVersion": "apsides.dev/ir/v2",
  "domain": {
    "id": "example",
    "namespace": "workload",
    "controlNamespace": "apsides-system",
    "clusterBinding": "local-fixture",
    "policyRef": "platform/example",
    "enforcement": "api-bound",
    "minKubernetesVersion": "1.34",
    "budgets": {"maxResources": 64, "maxInputs": 64, "maxReactions": 64, "maxObjectBytes": 262144, "maxQueueDepth": 128, "maxConcurrentRequests": 4}
  },
  "resources": [{
    "id": "config", "apiVersion": "v1", "kind": "ConfigMap", "name": "api-config", "namespace": "workload",
    "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "api-config", "namespace": "workload"}, "data": {"LOG_LEVEL": "info"}},
    "lifecycle": {"deletionPolicy": "orphan"},
    "source": {"file": "controller.ts", "line": 1, "column": 1}
  }],
  "inputs": [], "reactions": [], "handlers": [], "outputs": [],
  "provenance": {"compiler": "reference/0.1", "inputs": [{"path": "controller.ts", "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}]}
}
```

Managed resources support `ConfigMap`, `Service`, and `Deployment`, plus namespaced custom resources with an explicit `apiResource` mapping (`plural`, `scope: "Namespaced"`, `schemaSha256`). Their `desired` identity must exactly match the declaration. Builtins use a conservative schema subset and cannot override their REST mapping. Custom resources validate metadata, identity and plain JSON locally; the Kubernetes SSA dry-run validates the installed CRD schema. `schemaSha256` binds the fetched CRD source bytes used for type generation. Lifecycle policy is `orphan` or `delete`; actual deletion is deployment cleanup authority and is never a runtime effect. All resources and observations use the one workload namespace. Control namespace must differ.

A resource may carry `handlerOutput: true` (default `false`): it is the managed `ConfigMap` a handler `Output` compiles to (see below). Its `desired` is metadata-only — never a `data` key — and its `metadata.labels` must carry the reserved label `apsides.dev/handler-output` set to its own resource id; that label is rejected on every other resource, the same way `apsides.dev/revision` is reserved for the rollout primitive. The reconciler's own SSA of it, under field manager `apsides-<domainId>`, therefore never claims `data`; a later phase's handler writes attribute to the distinct field manager `apsides-handler-<domainId>`.

Inputs have `{id, apiVersion, kind, name, namespace, mode, managedResourceId?, projection, sensitive, missing, source}`. Kinds are `ConfigMap` or `Secret`, `apiVersion` is `v1`, mode is `managed`, `observed` or `output`, projection is exactly `["data"]`, and missing is exactly `block-dependent-mutations`. Managed inputs reference a declared ConfigMap with matching identity and use authoritative desired data. Observed inputs must not alias a managed resource. Secret inputs must be observed and sensitive. ConfigMap inputs are always non-sensitive, in every mode — including `output`, since a handler output can never carry a Secret. Fixture values are external to production IR.

`output`-mode inputs are auto-created for a handler `Output`, with `managedResourceId` naming its `handlerOutput` resource; they are non-sensitive `ConfigMap` inputs, exempt from the observed/managed aliasing rule (their identity intentionally matches their own output resource), and are observed live the same way as `observed` inputs — never treated as the reconciler's own authoritative desired state.

Reactions have `{id, op: "rollout", inputs: [inputId, ...], target: resourceId, source}`. Targets must be managed Deployments. Multiple reactions contributing to one target are combined into a single stable revision. Input IDs and reaction IDs are part of the canonical digest. The reserved annotation is `apsides.dev/revision`; source must not supply it. There are no generic expressions, callback operations, runtime names, adoption, delegated fields or readiness predicates yet; such requests fail validation.

## Handlers and outputs

`outputs` declares handler-written managed `ConfigMap`s: `{id, name, namespace}`. Each output id must name exactly one `handlerOutput` resource with a matching identity (kind `ConfigMap`, same `name`/`namespace`), and every `handlerOutput` resource must have a matching declared output — the two are always paired.

`handlers` declares Brigade-style event handlers, each a bundled TypeScript module invoked when its trigger input's revision changes:

```json
{
  "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
  "trigger": {"kind": "input-changed", "input": "settings"},
  "inputs": ["settings"],
  "egress": [{"host": "api.example.com", "port": 443}],
  "outputs": ["flags"],
  "timeoutMillis": 30000, "maxHeapBytes": 67108864,
  "bundleSha256": "…64 lowercase hex…",
  "source": {"file": "app.tsx", "line": 12, "column": 5}
}
```

`trigger.kind` is always `input-changed` in this increment; `trigger.input` must be one of `inputs`. Every id in `inputs` must resolve to a non-sensitive `ConfigMap` input (never `Secret`, never `sensitive: true`, whether `observed`, `managed` or `output` mode) — and a handler output is reachable *only* through its own `output`-mode input: a `managed`-mode input whose `managedResourceId` names a `handlerOutput` resource is rejected outright (it would read the reconciler's own static desired value, always `{}`, never the handler's live write). Every id in `outputs` must resolve to a declared output, and each output has exactly one writer handler across the whole domain — a second handler declaring the same output is rejected, since both would SSA-apply `data` under the same field manager and silently drop each other's keys. A handler may not trigger on or read (list it in `inputs`) an `output`-mode input backed by one of its own declared outputs — a self-loop — and the whole handler graph (a handler's outputs feeding another handler's inputs through an `output`-mode input) must be acyclic; either shape is a compile error. `module` is a normalized relative path (no leading slash, no `.`/`..` segment), resolved relative to the *source lock root* — `apsides.lock.json`'s own directory, the same root every path inside it is relative to — and must name one of `provenance.inputs`; `export` is a bare identifier; `bundleSha256` is the sha256 of the bundle the compiler wrote to `handlers/<id>.js` next to `ir.json`, embedded into the generated controller image with `include_str!`. `egress` hosts are lowercase DNS names — never an IP literal, a numeric IPv4 shorthand (decimal, hex or octal — anything that "ends in a number"), `localhost`, another single-label host, or a wildcard — reached only over HTTPS; `port` defaults to 443. Budgets: at most 16 handlers per domain, at most 16 distinct egress hosts across every handler combined, `timeoutMillis` in `1..=30000`, `maxHeapBytes` in `1..=67108864` (64 MiB).

Handler modules are authored as `<On trigger={input.changed} handler="./path.ts#exportName" inputs={[...]} egress={[...]} outputs={[...]} />` in the SDK; passing a function instead of a module-reference string is a compile error — handlers run in a separate execution context and cannot capture author closures. `useHandlerOutput({name})` declares an output and returns a projection usable in `rolloutOn`, exactly like an observed or managed input's projection; every declared output must have exactly one Output-mode input watching it (auto-created by `useHandlerOutput` itself), the invariant the output's named get/list/watch capability relies on. Handler modules must be listed in `apsides.lock.json`, resolved relative to the lock root, and may import only other locked modules — never an npm or remote specifier, never the authoring SDK itself.

Capabilities: an output's backing `output`-mode input adds named `get`/`list`/`watch` on its `ConfigMap`, exactly like an `observed` input; the `handlerOutput` resource itself adds the same `list`/`patch`/`create` capabilities every managed resource gets. Runtime execution of a handler's bundle (a fresh V8 isolate per invocation, its `apsides.fetch`/`apsides.writeOutput`/`apsides.log` ops) is implemented in the `engine` crate, embedded by `runtime` — dependency chain `cli` → `test-cluster` → `runtime` → `engine`, never the reverse. `apsides.writeOutput(id, key, data)` values (`data`, a `string` or `Uint8Array`) must be valid UTF-8 text: they land in the output `ConfigMap`'s `data` field (never `binaryData`), and a value that is not valid UTF-8 is rejected before anything is written. Each successful invocation commits its declared outputs as one server-side apply per output — the complete `data` map that output ends the invocation with, plus the `apsides.dev/handler-input-revision` annotation — never a series of per-key SSAs (those would delete each other's fields under one field manager's whole-field-set replace semantics).

Handler dispatch itself is level-based and serial: at most one handler invocation is dispatched per reconcile cycle, at global concurrency 1, so a busy handler queue never starves reconciles or inventory publishing; a handler is only dispatched once all of its declared outputs were successfully applied this cycle.

Ownership intents can never target a `handlerOutput` resource: it is either absent at first deploy (created absent-only by the reconciler, like any other managed resource with no prior owner) or already owned by this domain — adoption is meaningless for a resource only this domain's own handlers ever write to.

`validate(&Ir) -> Result<CompiledPlan>` independently derives effects, exact request strategy, runtime authorization, enforcement gaps, indirect risks, and semantic scenario plans. `parse_and_validate(&str)` also rejects duplicate JSON object keys. `CompiledPlan` fields use camelCase: `{irDigest, ir, effects, requestPlan, capabilities, enforcementGaps, indirectRisks, testPlan}`. Public `validate_ceiling(&CompiledPlan, &PermissionCeiling)` revalidates/recomputes the plan before checking scope and explicit risk/gap acceptance. Build identity is `sha256:` plus the canonical IR digest. Generation identity belongs to trusted deployment state, never the IR.

The named read strategy is get/list/watch with `metadata.name` field selectors on list/watch; every support request must appear separately. Managed targets use exact-name list and named patch plus unnamed create scope. Inputs separately require named get/list/watch. SSA create authority and broad patch authority require explicit ceiling acceptance. The first increment does not provide a production deployment executor.

Rollout evaluation accepts current input snapshots. Missing, unauthorized, unavailable and invalid observations block dependent mutation. Managed ConfigMap inputs require successful authoritative application. Sensitive combined revisions require a supplied domain key (at least 32 bytes) and stable key ID, use HMAC-SHA256, and never include secret data or the key in reports. Keys are not part of source, IR or generated artifacts.

The absent-only conditional SSA create contract (fixed ConfigMap, Service, and Deployment resources) is qualified against Kubernetes v1.34.3. `domain.minKubernetesVersion` declares the smallest server version a domain requires, as `"MAJOR.MINOR"`: major fixed at 1, minor at least 34 (`"1.34"` by default; authors may only raise it, never lower it). Deploy-time compatibility parses the connected server's discovery `major`/`minor` fields and requires them to be at or above the declared floor, rather than pinning an exact server version; the create-precondition mechanism itself remains qualified only on v1.34.3 regardless of a raised floor. A strongly consistent exact-name list must be empty and return a nonzero collection resourceVersion. Both server dry-run and real SSA carry that exact version, so an intervening creation produces a conflict instead of adoption. There is no resourceVersion-zero or forced fallback. Existing objects require Apply ownership and UID/resourceVersion preconditions. The request plan names the create-preflight list and conditional create-apply requests. This is a version-qualified built-in Kubernetes storage contract, not a claim about arbitrary aggregated APIs or other versions. Production still requires independent protected claims and durable UID inventory.

Generated test cases carry required `inputIds`, sorted to the scenario's applicable
reaction inputs. Common evaluation/replay/ownership/readiness cases bind all inputs;
observed-input changes, loss/recovery, denial and restart/relist bind only observed
inputs. Managed inputs instead require `managed-drift-repair` (workload),
`managed-missing`, `managed-read-denial` and `managed-unavailable` (API). Mixed
reactions retain both groups. Cases sort by reaction ID and scenario, independently
of declaration order. Semantic evaluation never rewrites managed desired IR to
simulate an external edit; actual managed repair and credential failures require
the Kubernetes runner and remain Unsupported in local semantic results.
