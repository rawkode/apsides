---
title: TSX authoring
description: Compose Kubernetes resources and declare observations with build-time components and hooks.
---

Author controllers as TSX component trees. Components run during compilation;
the cluster runs a native controller compiled from their declarations.

## Components and observations

```tsx
import {
  Controller, ConfigMap, Deployment,
  useConfigMap, useResourceRef, useSecret,
} from "@apsides/kubernetes";

const Api = ({ image }: { image: string }) => {
  const config = useResourceRef("ConfigMap");
  const settings = useConfigMap({ name: "external-settings" });
  const credentials = useSecret({ name: "database-credentials" });

  return <>
    <ConfigMap
      ref={config}
      metadata={{ name: "api-config", namespace: "production" }}
      data={{ LOG_LEVEL: "info" }}
    />
    <Deployment
      metadata={{ name: "api", namespace: "production" }}
      rolloutOn={[config.data, settings.data, credentials.data]}
      spec={{
        replicas: 3,
        selector: { matchLabels: { app: "api" } },
        template: {
          metadata: { labels: { app: "api" } },
          spec: { containers: [{ name: "api", image }] },
        },
      }}
    />
  </>;
};
```

Place `<Api image={...} />` inside a `<Controller>` with explicit identity,
namespace, policy and budgets. The [complete example](https://github.com/rawkode/apsides/tree/main/examples/web-application)
shows that boundary and the corresponding Service.

Resources accept Kubernetes-shaped `metadata` and `spec` props. Write reusable
components to encapsulate repeated workload configuration. Fragments group
resources without creating a Kubernetes object.

## Hooks declare inputs

`useConfigMap` and `useSecret` declare named observations in the controller's
namespace. Their `.data` properties are opaque projection handles, not live
values. They cannot be read to choose resources, inspect secrets or execute
callbacks. A missing observation blocks its dependent mutations.

`useResourceRef("ConfigMap")` creates a handle bound by a ConfigMap component's
`ref` prop. Its `.data` projection can also trigger a rollout. The compiler
resolves these relationships after traversing the tree, so a target can precede
its input declaration. Unbound or foreign references are rejected.

Hooks must run synchronously inside a component under a Controller. Components
cannot return promises. State and effect hooks, browser APIs, runtime JavaScript
and arbitrary runtime callbacks are outside this authoring contract.

## Force and deletion

Controller `force` and `prune` options set defaults. Resources may override
`force` and `deletionPolicy` independently. Force requests SSA field ownership
takeover subject to policy; it does not bypass adoption or UID checks. A removed
resource's previous deletion policy controls whether cleanup deletes or orphans it.

## Compiler-owned metadata

Application components do not supply source digests, source locations or test
fixtures. The compiler captures the locked source closure and SDK modules,
typechecks and lowers TSX with Oxc, and evaluates the tree in isolated Deno/SES
processes. Only validated data and fixed rollout reactions enter the native IR.

Resource locations identify JSX declarations. Hook locations currently identify
the containing component invocation, not the hook expression itself. Source
locations assist diagnostics; they do not confer authority.

See the [IR contract](/reference/ir-contract/) and [toolchain](/reference/toolchain/) for
the underlying constraints. The [implementation status](/reference/status/) records
the remaining production work.
