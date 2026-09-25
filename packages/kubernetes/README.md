# `@apsides/kubernetes`

Apsides uses TSX components and hooks to describe a fixed Kubernetes controller. Components execute only during isolated compilation. The compiler lowers their tree into the existing validated native IR; runtime reconciliation never calls back into them. A controller runs JavaScript only for the handler modules `<On>` elements declare: each is bundled at compile time and executed in production mode by the controller's embedded engine, with access only to its declared inputs, egress hosts and outputs.

```tsx
const Api = () => {
  const config = useResourceRef("ConfigMap");
  const credentials = useSecret({ name: "credentials" });
  return <>
    <ConfigMap ref={config} metadata={{ name: "config", namespace: "workload" }} data={{ level: "info" }} />
    <Deployment {...deploymentDefinition} rolloutOn={[config.data, credentials.data]} />
  </>;
};

export default <Controller {...domainOptions}><Api /></Controller>;
```

Import components and hooks from `@apsides/kubernetes`. `Controller` accepts explicit domain options: identity, workload/control namespaces, cluster binding, policy, enforcement, budgets, and optional force/prune defaults. Provenance is supplied by the compiler. `ConfigMap`, `Service`, and `Deployment` use the conservative Kubernetes-shaped definitions documented in the IR contract, plus optional `force` and `deletionPolicy` props. Deployment alone accepts `rolloutOn`.

`useConfigMap({name, namespace?, id?})` and `useSecret(...)` declare named observed inputs. Their `.data` properties are opaque projection tokens, not values. `useResourceRef("ConfigMap")` creates a managed reference whose `.data` can participate in `rolloutOn`; bind it exactly once with `<ConfigMap ref={reference} ... />`. Deployment and Service refs are also supported for binding but have no data projection. Unbound, reused, wrong-kind, and foreign-render references fail compilation. Managed ConfigMap drift remains authoritative desired state, not a new accepted external revision.

Hooks run synchronously in custom components inside one `Controller`. JSX factories are inert; compiler traversal establishes the domain before invoking components. Forward references resolve after the complete tree is visited. Composition, fragments, arrays, and finite build-time conditionals are supported. Async components, runtime effects/state, callbacks in resource fields, dynamic runtime resource creation, and nested/multiple controllers are unsupported. Every build emits a fixed graph; changes to its identities or effects require another build.

Oxc supplies resource and rollout JSX locations. Hook input locations identify the containing component's JSX invocation, not the hook statement itself. Source maps use paths relative to the locked source root. Locations aid diagnostics; they are not an authorization boundary.

The compiler captures and verifies the complete SDK module closure and author source lock, then evaluates twice in a pinned SES Compartment hosted by Deno with denied host permissions. Native validation independently checks the resulting IR. Projection tokens cannot enter native resource fields, and observed Secret values never enter authoring evaluation. Compiler fixture generation supplies separate synthetic inputs and workload images.

The compiler renderer and validation model are internal and are not public package exports.
