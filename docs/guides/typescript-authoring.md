---
title: TypeScript authoring
description: Declare bounded Kubernetes resources, observations and rollout reactions.
---

The `@reakt/kubernetes` package describes a fixed domain. Authoring code runs
during compilation and emits data; it is not shipped as a cluster-side evaluator.

## A minimal domain

```ts
import {
  ConfigMap,
  Deployment,
  defineDomain,
  watch,
} from "@reakt/kubernetes";

const source = { file: "program.ts", line: 1, column: 1 };

const domain = defineDomain({
  id: "team-api-production",
  namespace: "production",
  controlNamespace: "reakt-system",
  clusterBinding: "production-cluster",
  policyRef: "platform/team-api",
  enforcement: "api-bound",
  budgets: {
    maxResources: 16,
    maxInputs: 16,
    maxReactions: 16,
    maxObjectBytes: 262_144,
    maxQueueDepth: 128,
    maxConcurrentRequests: 4,
  },
  provenance: {
    inputs: [{ path: "program.ts", sha256: "<supplied-by-build-wrapper>" }],
  },
  force: false,
  prune: false,
});

const settings = domain.observe(ConfigMap, {
  id: "settings",
  name: "api-settings",
  namespace: "production",
  projection: ["data"],
  missing: "block-dependent-mutations",
  source,
});

const api = domain.manage(new Deployment({
  id: "api",
  metadata: { name: "api", namespace: "production" },
  spec: {
    replicas: 3,
    selector: { matchLabels: { app: "api" } },
    template: {
      metadata: { labels: { app: "api" } },
      spec: {
        containers: [{
          name: "api",
          image: "registry.example.com/api@sha256:<digest>",
        }],
      },
    },
  },
}), {
  deletionPolicy: "orphan",
  force: false,
  source,
});

watch(settings).project("data").rollout(api, {
  id: "settings-rollout",
  source,
});

export default domain.toIR();
```

Use the checked-in [complete example](https://github.com/rawkode/apsides/tree/main/examples/reakt)
for executable source. The abbreviated snippet above shows the shape of the API;
the build wrapper supplies real source digests and locations.

## Resources and observations

The current conservative resource surface supports `ConfigMap`, `Service` and
`Deployment`. Observations support named `ConfigMap` and `Secret` inputs with a
`data` projection. Sensitive input values are used to derive reactions but do
not enter production IR.

All names are fixed at build time and must stay within the domain namespace.
Unknown fields, cross-domain writes, the reserved `reakt.dev/revision`
annotation, ambient accessors and unsupported resource shapes fail validation.

## Reactions

`watch(input).project("data").rollout(deployment)` declares a fixed reaction.
When the selected data changes, the runtime computes a deterministic revision
and applies the declared Deployment. It does not invoke an author callback.

Missing observed input uses the only current policy,
`block-dependent-mutations`: dependent writes pause rather than guessing a
value.

## Force and deletion policy

The domain's `force` and `prune` options set defaults. Each managed resource can
override them:

```ts
domain.manage(resource, {
  force: true,
  deletionPolicy: "delete",
  source,
});
```

`force: true` requests SSA field-ownership takeover and creates a separately
reviewed policy risk. `deletionPolicy: "delete"` authorizes only the later
UID-checked cleanup path. Neither option grants adoption or bypasses platform
policy.

## Source and fixture boundaries

The normal source build records every declared input and compares independent
bundle/evaluation runs. Test fixtures are explicit, carry `testOnly: true` and
`productionReady: false`, and cannot be promoted as production artifacts.

See the [IR contract](/reakt/ir-contract/) for the accepted schema and the
[toolchain](/reakt/toolchain/) for compiler isolation details.
