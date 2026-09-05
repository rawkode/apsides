# `@reakt/kubernetes`

This package is the additive TypeScript authoring surface for the initial Reakt IR. TypeScript runs only while building declarations; `.toIR()` contains data and the fixed `rollout` operation, never callbacks.

```ts
const domain = defineDomain({
  id: "team-api-production",
  namespace: "production",
  controlNamespace: "reakt-system",
  clusterBinding: "production-cluster",
  policyRef: "platform/team-api",
  enforcement: "api-bound",
  budgets,
  provenance: { inputs: [{ path: "domain.ts", sha256 }] },
});

const config = domain.manage(new ConfigMap({
  id: "config",
  metadata: { name: "api-config", namespace: "production" },
  data: { LOG_LEVEL: "info" },
}), { deletionPolicy: "orphan", source: { file: "domain.ts", line: 1, column: 1 } });

// `api` is a managed Deployment in this domain.
watch(config).project("data").rollout(api);
const ir = domain.toIR();
// ir.testOnly is always false; only the test compiler can set it true.
```

All names are fixed and must use the domain namespace. Sources must name a declared provenance input. `ConfigMap`, `Service`, and `Deployment` accept a conservative Kubernetes subset. Observations are named `ConfigMap` or `Secret` inputs with the `data` projection and block dependent mutations when missing. Cross-domain writes, unknown fields, ambient accessor values, non-data values, and the reserved `reakt.dev/revision` annotation are rejected.

`generateFixtureBundle(ir, { workloadImage: "reakt-fixture:local" })` derives synthetic observed ConfigMap/Secret fixtures and complete Deployment container substitutions from the production IR. Explicit `defineTestFixture`, `defineWorkloadFixture`, and `defineFixtureBundle` declarations remain available for supported overrides. Test inputs carry a `synthetic: true` assertion. `materializeFixtureIR` changes the IR marker to `testOnly: true`; fixture values never enter production IR.

This SDK validates declarations. The source compiler supplies the locked input closure, a pinned SES Compartment with only the hardened declared input, denied Deno host permissions, repeated evaluation, and provenance replacement described in `docs/reakt/TOOLCHAIN.md`.
