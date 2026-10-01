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
namespace, policy and budgets. `minKubernetesVersion` optionally raises the
smallest Kubernetes version the domain accepts, as `"MAJOR.MINOR"`; it
defaults to `"1.34"` and cannot be lower. `aps deploy` refuses a cluster
whose server version is below it. The [complete example](https://github.com/rawkode/apsides/tree/main/examples/web-application)
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
cannot return promises. State and effect hooks, browser APIs and arbitrary
runtime callbacks are outside this authoring contract. The only code that runs
in the cluster is a declared event handler module.

## Event handlers

`<On>` runs a handler module when an input changes, and `useHandlerOutput`
declares the managed `ConfigMap` it writes:

```tsx
const settings = useConfigMap({ id: "feature-settings", name: "feature-settings" });
const flags = useHandlerOutput({ id: "resolved-flags", name: "resolved-flags" });

<On
  trigger={settings.changed}
  handler="./handlers/resolve-flags.ts#resolveFlags"
  inputs={[settings]}
  egress={["flags.example.com"]}
  outputs={[flags]}
/>
```

- `handler` is a `"<module path>#<export>"` string, never a function. The path
  is resolved from the directory holding `apsides.lock.json`.
- The module must be listed in `apsides.lock.json`
  (`aps lock --source program.tsx --modules handlers/resolve-flags.ts`) and
  may import only other locked modules through relative specifiers, plus
  `import type` from `@apsides/kubernetes/handler`. `aps compile` type-checks
  it in strict mode against the ECMAScript library and those declarations,
  and checks that the export accepts the event its `<On>` produces.
- Inputs are non-sensitive `ConfigMap` observations or references; Secrets
  cannot reach a handler. The trigger must be one of the inputs.
- `egress` lists the HTTPS hosts the handler may call; `outputs` lists the
  outputs it writes, and each output has exactly one writer.
- `flags.data` can be used in a Deployment's `rolloutOn`, and `flags.changed`
  can trigger another `<On>` that lists `flags` in its `inputs`, chaining
  handlers.

At runtime the export receives the current input data and may call
`apsides.fetch`, `apsides.writeOutput` and `apsides.log`. See
[Event handlers](/guides/event-handlers/) for the API, limits, delivery and
security model.

## Force and deletion

Controller `force` and `prune` options set defaults. Resources may override
`force` and `deletionPolicy` independently. Force requests SSA field ownership
takeover subject to policy; it does not bypass adoption or UID checks. A removed
resource's previous deletion policy controls whether cleanup deletes or orphans it.

## Compiler-owned metadata

Application components do not supply source digests, source locations or test
fixtures. The compiler captures the locked source closure and SDK modules,
typechecks and lowers TSX with Oxc, and evaluates the tree in isolated Deno/SES
processes. Only validated data, fixed rollout reactions and handler
declarations enter the native IR. Handler modules are not evaluated at build
time: the compiler bundles each one and records its digest.

Resource locations identify JSX declarations. Hook locations currently identify
the containing component invocation, not the hook expression itself. Source
locations assist diagnostics; they do not confer authority.

See the [IR contract](/reference/ir-contract/) and [toolchain](/reference/toolchain/) for
the underlying constraints. The [implementation status](/reference/status/) records
the remaining production work.
