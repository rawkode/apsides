# Apsides IR contract

This is the current implementation boundary for specification 0.2. It is not a claim of production readiness. Unknown fields and unsupported operations are errors.

`schemaVersion` is `apsides.dev/ir/v1`. JSON uses camelCase. Every declaration has `source: {file: string, line: positive integer, column: positive integer}`. Resource and input IDs are unique within their category. A reaction references input IDs and a managed resource ID.

```json
{
  "schemaVersion": "apsides.dev/ir/v1",
  "domain": {
    "id": "example",
    "namespace": "workload",
    "controlNamespace": "apsides-system",
    "clusterBinding": "local-fixture",
    "policyRef": "platform/example",
    "enforcement": "api-bound",
    "budgets": {"maxResources": 64, "maxInputs": 64, "maxReactions": 64, "maxObjectBytes": 262144, "maxQueueDepth": 128, "maxConcurrentRequests": 4}
  },
  "resources": [{
    "id": "config", "apiVersion": "v1", "kind": "ConfigMap", "name": "api-config", "namespace": "workload",
    "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "api-config", "namespace": "workload"}, "data": {"LOG_LEVEL": "info"}},
    "lifecycle": {"deletionPolicy": "orphan"},
    "source": {"file": "controller.ts", "line": 1, "column": 1}
  }],
  "inputs": [], "reactions": [],
  "provenance": {"compiler": "reference/0.1", "inputs": [{"path": "controller.ts", "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}]}
}
```

Managed resources support `ConfigMap`, `Service`, and `Deployment`, plus namespaced custom resources with an explicit `apiResource` mapping (`plural`, `scope: "Namespaced"`, `schemaSha256`). Their `desired` identity must exactly match the declaration. Builtins use a conservative schema subset and cannot override their REST mapping. Custom resources validate metadata, identity and plain JSON locally; the Kubernetes SSA dry-run validates the installed CRD schema. `schemaSha256` binds the fetched CRD source bytes used for type generation. Lifecycle policy is `orphan` or `delete`; actual deletion is deployment cleanup authority and is never a runtime effect. All resources and observations use the one workload namespace. Control namespace must differ.

Inputs have `{id, apiVersion, kind, name, namespace, mode, managedResourceId?, projection, sensitive, missing, source}`. Kinds are `ConfigMap` or `Secret`, `apiVersion` is `v1`, mode is `managed` or `observed`, projection is exactly `["data"]`, and missing is exactly `block-dependent-mutations`. Managed inputs reference a declared ConfigMap with matching identity and use authoritative desired data. Observed inputs must not alias a managed resource. Secret inputs must be observed and sensitive. ConfigMap inputs must be non-sensitive in this initial increment. Fixture values are external to production IR.

Reactions have `{id, op: "rollout", inputs: [inputId, ...], target: resourceId, source}`. Targets must be managed Deployments. Multiple reactions contributing to one target are combined into a single stable revision. Input IDs and reaction IDs are part of the canonical digest. The reserved annotation is `apsides.dev/revision`; source must not supply it. There are no generic expressions, callback operations, event actions, runtime names, adoption, delegated fields or readiness predicates yet; such requests fail validation.

`validate(&Ir) -> Result<CompiledPlan>` independently derives effects, exact request strategy, runtime authorization, enforcement gaps, indirect risks, and semantic scenario plans. `parse_and_validate(&str)` also rejects duplicate JSON object keys. `CompiledPlan` fields use camelCase: `{irDigest, ir, effects, requestPlan, capabilities, enforcementGaps, indirectRisks, testPlan}`. Public `validate_ceiling(&CompiledPlan, &PermissionCeiling)` revalidates/recomputes the plan before checking scope and explicit risk/gap acceptance. Build identity is `sha256:` plus the canonical IR digest. Generation identity belongs to trusted deployment state, never the IR.

The named read strategy is get/list/watch with `metadata.name` field selectors on list/watch; every support request must appear separately. Managed targets use exact-name list and named patch plus unnamed create scope. Inputs separately require named get/list/watch. SSA create authority and broad patch authority require explicit ceiling acceptance. The first increment does not provide a production deployment executor.

Rollout evaluation accepts current input snapshots. Missing, unauthorized, unavailable and invalid observations block dependent mutation. Managed ConfigMap inputs require successful authoritative application. Sensitive combined revisions require a supplied domain key (at least 32 bytes) and stable key ID, use HMAC-SHA256, and never include secret data or the key in reports. Keys are not part of source, IR or generated artifacts.

The fixture creation path is qualified for fixed ConfigMap, Service, and Deployment resources on Kubernetes 1.34.3. A strongly consistent exact-name list must be empty and return a nonzero collection resourceVersion. Both server dry-run and real SSA carry that exact version, so an intervening creation produces a conflict instead of adoption. There is no resourceVersion-zero or forced fallback. Existing objects require Apply ownership and UID/resourceVersion preconditions. The request plan names the create-preflight list and conditional create-apply requests. This is a version-qualified built-in Kubernetes storage contract, not a claim about arbitrary aggregated APIs or other versions. Production still requires independent protected claims and durable UID inventory.

Generated test cases carry required `inputIds`, sorted to the scenario's applicable
reaction inputs. Common evaluation/replay/ownership/readiness cases bind all inputs;
observed-input changes, loss/recovery, denial and restart/relist bind only observed
inputs. Managed inputs instead require `managed-drift-repair` (workload),
`managed-missing`, `managed-read-denial` and `managed-unavailable` (API). Mixed
reactions retain both groups. Cases sort by reaction ID and scenario, independently
of declaration order. Semantic evaluation never rewrites managed desired IR to
simulate an external edit; actual managed repair and credential failures require
the Kubernetes runner and remain Unsupported in local semantic results.
