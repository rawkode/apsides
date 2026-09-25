# Apsides IR contract

This is the current implementation boundary for the [specification](SPEC.md). It is not a claim of production readiness. Unknown fields and unsupported operations are errors.

`schemaVersion` is `apsides.dev/ir/v2`. JSON uses camelCase. Every declaration has `source: {file: string, line: positive integer, column: positive integer}`. Resource, input, reaction, handler and output IDs are unique within their category. A reaction references input IDs and a managed resource ID.

This is the `ir.json` that `aps compile` writes for `crates/acceptance/programs/handler-output/program.tsx`: one observed input, one handler and its output.

```json
{
  "testOnly": false,
  "schemaVersion": "apsides.dev/ir/v2",
  "domain": {
    "id": "handler-output",
    "namespace": "default",
    "controlNamespace": "apsides-system",
    "clusterBinding": "local-demo",
    "policyRef": "platform/handler-output",
    "enforcement": "api-bound",
    "budgets": {
      "maxResources": 1,
      "maxInputs": 2,
      "maxReactions": 1,
      "maxObjectBytes": 262144,
      "maxQueueDepth": 8,
      "maxConcurrentRequests": 1
    },
    "minKubernetesVersion": "1.34"
  },
  "resources": [
    {
      "id": "handler-derived",
      "apiVersion": "v1",
      "kind": "ConfigMap",
      "name": "handler-derived",
      "namespace": "default",
      "desired": {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
          "labels": {"apsides.dev/handler-output": "handler-derived"},
          "name": "handler-derived",
          "namespace": "default"
        }
      },
      "lifecycle": {"deletionPolicy": "orphan"},
      "source": {"file": "program.tsx", "line": 38, "column": 5},
      "handlerOutput": true
    }
  ],
  "inputs": [
    {
      "id": "handler-source",
      "apiVersion": "v1",
      "kind": "ConfigMap",
      "name": "handler-source",
      "namespace": "default",
      "mode": "observed",
      "projection": ["data"],
      "sensitive": false,
      "missing": "block-dependent-mutations",
      "source": {"file": "program.tsx", "line": 38, "column": 5}
    },
    {
      "id": "output-input-handler-derived",
      "apiVersion": "v1",
      "kind": "ConfigMap",
      "name": "handler-derived",
      "namespace": "default",
      "mode": "output",
      "managedResourceId": "handler-derived",
      "projection": ["data"],
      "sensitive": false,
      "missing": "block-dependent-mutations",
      "source": {"file": "program.tsx", "line": 38, "column": 5}
    }
  ],
  "reactions": [],
  "handlers": [
    {
      "id": "handler-derivekeys---handlers-derive-keys-ts-c9d54577a4",
      "module": "handlers/derive-keys.ts",
      "export": "deriveKeys",
      "trigger": {"kind": "input-changed", "input": "handler-source"},
      "inputs": ["handler-source"],
      "egress": [],
      "outputs": ["handler-derived"],
      "timeoutMillis": 30000,
      "maxHeapBytes": 67108864,
      "bundleSha256": "64ed2048396911f60fedf793e28d1ea0a0a8e10b0f7d95f4e7d919e6662dc94a",
      "source": {"file": "program.tsx", "line": 11, "column": 5}
    }
  ],
  "outputs": [
    {"id": "handler-derived", "name": "handler-derived", "namespace": "default"}
  ],
  "provenance": {
    "compiler": "apsides/0.1",
    "inputs": [
      {
        "path": "handlers/derive-keys.ts",
        "sha256": "dafb1ac90f274c44cf148603d135cdca7be0daa563a0c163b049adb0982a9da1"
      },
      {"path": "program.tsx", "sha256": "1113c4fa8325139d87a0b1e0ebd3270565fb08adacfb61f7f1e6a93eea108087"}
    ]
  }
}
```

`testOnly` is `false` in production IR; only the compiler's own fixture IR sets it, and release verification refuses it. `domain.minKubernetesVersion` is required; see [Kubernetes version](#kubernetes-version).

Managed resources support `ConfigMap`, `Service`, and `Deployment`, plus namespaced custom resources with an explicit `apiResource` mapping (`plural`, `scope: "Namespaced"`, `schemaSha256`). Their `desired` identity must exactly match the declaration. Builtins use a conservative schema subset and cannot override their REST mapping. Custom resources validate metadata, identity and plain JSON locally; the Kubernetes SSA dry-run validates the installed CRD schema. `schemaSha256` binds the fetched CRD source bytes used for type generation. Lifecycle policy is `orphan` or `delete`; actual deletion is deployment cleanup authority and is never a runtime effect. All resources and observations use the one workload namespace. Control namespace must differ.

A resource may carry `handlerOutput: true` (default `false`): it is the managed `ConfigMap` a handler output compiles to (see below). Its `desired` is metadata-only — never a `data` key — and its `metadata.labels` must carry the reserved label `apsides.dev/handler-output` set to its own resource id; that label is rejected on every other resource, the same way `apsides.dev/revision` is reserved for the rollout primitive. The reconciler's own SSA of it, under field manager `apsides-<domainId>`, therefore never claims `data`; handler writes use the distinct field manager `apsides-handler-<domainId>`.

Inputs have `{id, apiVersion, kind, name, namespace, mode, managedResourceId?, projection, sensitive, missing, source}`. Kinds are `ConfigMap` or `Secret`, `apiVersion` is `v1`, mode is `managed`, `observed` or `output`, projection is exactly `["data"]`, and missing is exactly `block-dependent-mutations`. Managed inputs reference a declared ConfigMap with matching identity and use authoritative desired data. Observed inputs must not alias a managed resource. Secret inputs must be observed and sensitive. ConfigMap inputs are always non-sensitive, in every mode. Fixture values are external to production IR.

`output`-mode inputs are created by `useHandlerOutput` for each handler output, with `managedResourceId` naming its `handlerOutput` resource. They are non-sensitive `ConfigMap` inputs, exempt from the observed/managed aliasing rule (their identity intentionally matches their own output resource), and are observed live the same way as `observed` inputs — never treated as the reconciler's own authoritative desired state. A `managed`-mode input may not reference a `handlerOutput` resource: it would read the reconciler's static desired value, which never carries `data`.

Reactions have `{id, op: "rollout", inputs: [inputId, ...], target: resourceId, source}`. Targets must be managed Deployments. Any input mode may feed a reaction, including an `output`-mode input, so a handler's written data can roll a Deployment out. Multiple reactions contributing to one target are combined into a single stable revision. Input IDs and reaction IDs are part of the canonical digest. The reserved annotation is `apsides.dev/revision`; source must not supply it. There are no generic expressions, callback operations, runtime names, adoption, delegated fields or readiness predicates; such requests fail validation.

## Handlers and outputs

`outputs` declares handler-written managed `ConfigMap`s: `{id, name, namespace}`. Each output id must name exactly one `handlerOutput` resource with a matching identity (kind `ConfigMap`, same `name`/`namespace`), every `handlerOutput` resource must have a matching declared output, and every output must have exactly one `output`-mode input watching it.

`handlers` declares event handlers: `{id, module, export, trigger, inputs, egress, outputs, timeoutMillis, maxHeapBytes, bundleSha256, source}`. Each is a bundled module export invoked when its trigger input's revision changes. A declared egress entry has the shape `{"host": "flags.example.com", "port": 443}`.

- `trigger` is `{kind: "input-changed", input}`; `input-changed` is the only trigger kind, and `trigger.input` must be one of `inputs`.
- `inputs` is a nonempty list of distinct input ids. Each must resolve to a non-sensitive `ConfigMap` input in any mode. Secret inputs can never reach a handler. A handler output is readable only through its own `output`-mode input.
- `outputs` lists distinct declared output ids. Each output has exactly one writer handler across the domain; a second writer is rejected, since both would apply `data` under the same field manager and drop each other's keys.
- A handler may not trigger on or read its own output, and the handler graph (a handler's outputs feeding another handler's inputs through an `output`-mode input) must be acyclic.
- `module` is a normalized relative path (no leading slash, backslash, `.` or `..` segment), resolved from the source lock root (the directory holding `apsides.lock.json`), and must be one of `provenance.inputs`. `export` is a bare identifier.
- `bundleSha256` is the SHA-256 of the handler bundle the compiler writes to `controller/src/handlers/<id>.js` beside the generated controller's embedded `ir.json`. The generated controller embeds each bundle and refuses to start unless the embedded bundles match the IR's handlers exactly.
- `egress` entries are `{host, port}`. A host is a lowercase DNS name with at least one dot. IP literals, numeric IPv4 shorthands (decimal, hex or octal, anything whose last label is numeric), `localhost`, names under `.localhost`, single-label names and wildcards are rejected. `port` defaults to 443 and each `(host, port)` pair is distinct within a handler. Egress is always HTTPS.
- Budgets: at most 16 handlers per domain, at most 16 distinct egress hosts across all handlers, at most 256 `(host, port, handler)` egress rows, `timeoutMillis` in `1..=30000` (default 30000), and `maxHeapBytes` in `1..=67108864` (64 MiB, the default).

The authoring surface is `<On trigger={input.changed} handler="./path.ts#exportName" inputs={[...]} egress={[...]} outputs={[...]} />` and `useHandlerOutput({id?, name, namespace?})`; the [event handler guide](/guides/event-handlers/) describes it. Passing a function as `handler` is a compile error. Handler modules must be listed in `apsides.lock.json` and may import only other locked modules through relative specifiers — never an npm or remote specifier, never the authoring SDK, and never a dynamic import. The compiler bundles each handler twice and requires byte-identical output.

Capabilities: an output's `output`-mode input adds named `get`/`list`/`watch` on its `ConfigMap`, exactly like an `observed` input; the `handlerOutput` resource itself adds the same `list`/`patch`/`create` capabilities every managed resource gets. Egress hosts are not Kubernetes capabilities; they appear in `surface.json` and the deployment review.

### Output commit

`apsides.writeOutput(id, key, data)` calls are buffered for the invocation. Values (`string` or `Uint8Array`) must be valid UTF-8 text: they land in the output `ConfigMap`'s `data` field (never `binaryData`). Keys match `[-._a-zA-Z0-9]+`, are at most 253 characters and are never `.` or `..`. The key and value bytes written to one output in one invocation may total at most the domain's `maxObjectBytes`.

Only when the handler's promise resolves does the runtime commit its outputs: one server-side apply per declared output, under field manager `apsides-handler-<domainId>`, carrying the written keys and the `apsides.dev/handler-input-revision` annotation. The apply goes through the same authority and claim re-check reconciler mutations use, and is bound to the output's UID as observed when the cycle applied it; a replaced object fails the commit with `handler-output-replaced`. Because one field manager's apply replaces its whole field set:

- the keys written in the last invocation that wrote an output replace all of the handler's own keys in it; a key the handler owned but did not write again is dropped;
- keys owned by another field manager (a key added with `kubectl apply` or `kubectl edit`) are never touched and always survive;
- an invocation that does not write to a declared output leaves the handler's keys in it unchanged: the output is re-applied with exactly the keys the handler manager currently owns, read from its `managedFields`, to stamp the new revision. A handler cannot empty an output by not writing to it, and cannot become a co-owner of a foreign key.

### Dispatch

Dispatch is level-based and serial. Each cycle computes every handler's input revision, a digest of the handler id, bundle digest, export, outputs, trigger and the data of every declared input. A handler is enqueued when that revision differs from the one its outputs record, or when an output was deleted, recreated or lost its annotation. A queued handler whose revision changes again is coalesced to the latest event. At most one invocation is dispatched per reconcile cycle, at global concurrency 1, and only once all of the handler's outputs were applied in that cycle; otherwise it reports `handler-output-not-ready`. The queue holds at most `maxQueueDepth` handlers; a new handler beyond that reports `handler-queue-overflow`. A failed invocation or commit reports `handler-invocation-failed:<CODE>` and is re-queued with the same event under exponential backoff (the controller's `baseBackoffMillis`/`maxBackoffMillis`), so delivery is at least once. Fixture-mode controllers verify the embedded bundles but do not execute handlers.

Ownership intents can never target a `handlerOutput` resource: it is either absent at first deploy (created absent-only by the reconciler, like any other managed resource with no prior owner) or already owned by this domain.

## Validation and request plan

`validate(&Ir) -> Result<CompiledPlan>` independently derives effects, exact request strategy, runtime authorization, enforcement gaps, indirect risks, and semantic scenario plans. `parse_and_validate(&str)` also rejects duplicate JSON object keys. `CompiledPlan` fields use camelCase: `{irDigest, ir, effects, requestPlan, capabilities, enforcementGaps, indirectRisks, testPlan}`. Public `validate_ceiling(&CompiledPlan, &PermissionCeiling)` revalidates/recomputes the plan before checking scope and explicit risk/gap acceptance. Build identity is `sha256:` plus the canonical IR digest. Generation identity belongs to trusted deployment state, never the IR.

The named read strategy is get/list/watch with `metadata.name` field selectors on list/watch; every support request must appear separately. Managed targets use exact-name list and named patch plus unnamed create scope. Inputs separately require named get/list/watch. SSA create authority and broad patch authority require explicit ceiling acceptance.

Rollout evaluation accepts current input snapshots. Missing, unauthorized, unavailable and invalid observations block dependent mutation. Managed ConfigMap inputs require successful authoritative application. Sensitive combined revisions require a supplied domain key (at least 32 bytes) and stable key ID, use HMAC-SHA256, and never include secret data or the key in reports. Keys are not part of source, IR or generated artifacts.

## Kubernetes version

`domain.minKubernetesVersion` declares the smallest server version a domain requires, as canonical `"MAJOR.MINOR"`: major fixed at 1, minor at least 34. `<Controller minKubernetesVersion>` defaults to `"1.34"` and may only raise it. `aps deploy` and live preview read the connected server's discovery `major`/`minor` fields (ignoring a non-digit minor suffix such as `+`) and refuse a server below the declared floor. `surface.json` records it as `kubernetes.minVersion` and `compatibility.json` as `kubernetesVersion.minVersion`, each next to `qualifiedOn: "v1.34.3"`.

The absent-only conditional SSA create contract (fixed ConfigMap, Service, and Deployment resources) is qualified against Kubernetes v1.34.3, whatever the declared floor. A strongly consistent exact-name list must be empty and return a nonzero collection resourceVersion. Both server dry-run and real SSA carry that exact version, so an intervening creation produces a conflict instead of adoption. There is no resourceVersion-zero or forced fallback. Existing objects require Apply ownership and UID/resourceVersion preconditions. The request plan names the create-preflight list and conditional create-apply requests. This is a version-qualified built-in Kubernetes storage contract, not a claim about arbitrary aggregated APIs or other versions. Production still requires independent protected claims and durable UID inventory.

## Generated test cases

Generated test cases carry required `inputIds`, sorted to the scenario's applicable
reaction inputs. Common evaluation/replay/ownership/readiness cases bind all inputs;
observed-input changes, loss/recovery, denial and restart/relist bind only observed
inputs. Managed inputs instead require `managed-drift-repair` (workload),
`managed-missing`, `managed-read-denial` and `managed-unavailable` (API). Mixed
reactions retain both groups. Cases sort by reaction ID and scenario, independently
of declaration order. `output`-mode inputs share the live-input `projected-change`
and `restart-and-relist` cases with observed inputs, but not the missing,
returned, unauthorized, credential-denial, unavailable, invalid or coalesced-input
cases: the reconciler owns the output object and its read authority, and an output
has exactly one writer. Semantic evaluation never rewrites managed desired IR to
simulate an external edit; actual managed repair and credential failures require
the Kubernetes runner and remain Unsupported in local semantic results.
