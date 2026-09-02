# Controller TSX v1 contract

This contract is frozen by the canonical fixture in
`packages/fixtures/golden/v1/controller.valid.tsx`. It is a deterministic
Kubernetes declaration language, not a general-purpose UI runtime.

## One controller module

Every v1 program is one `controller.tsx` module. It imports its entire public
surface from `@oxideloop/kubernetes`, declares its root `WebApplicationSpec`,
has one non-exported direct top-level action binding, and has one direct default
controller definition:

```tsx
interface WebApplicationSpec { image: string; replicas?: number; expose?: boolean }
const configurationChanged = defineAction<{ message: string }>({ /* ... */ });
export default defineController<WebApplicationSpec>(({ root }) => { /* ... */ });
```

The compiler permits only allowlisted static component imports, this interface,
the direct `const configurationChanged = defineAction(...)` binding, and the
direct `export default defineController(...)` definition. It rejects authored
relative or dynamic imports, aliases, reassignment, conditional phase
definitions, module initializers, and other exports or top-level statements.

The action is compiled first. The render callable receives the exact action ID
and digest produced by that compilation. `defineAction` receives an object with
an immutable `id`, `version`, and `run` callback. The callback may capture only
parameters, values derived from parameters, literals, and approved component
imports. It cannot be async or a generator; read ambient globals; write a
nonlocal; or call an unknown value. Its only result is an `<Event>`.

## Controller capabilities

The root exposes plain JSON `root.metadata` and `root.spec` properties. The
canonical controller derives `name`, `configName`, and `labels` using ordinary
TypeScript values. `useResource(ConfigMap, { name: "demo-input" })` returns a
resource handle whose current value is available through `.current`; it does
not take a raw GVK reference.

`handle.changed()` is an opaque, session-bound automation trigger. It can only
be passed as `when` in the object form of `useAutomation`:

```tsx
useAutomation({ when: settings.changed(), action: configurationChanged, metadata: { message } });
```

An absent source produces no automation. Initial presence creates a durable
baseline and no Event; each later exact source transition creates at most one
Event. The action identity and version are immutable for that behavior.

`useEnrichment({ labels, podSecurityContext })` returns a `<Defaults>` wrapper.
Its configuration is strict-JSON snapshot-cloned and deep-frozen. It fills only
absent scalar or top-level properties, nearest wrapper wins, and it never
changes `id`, `name`, namespace, children, or arrays.

## Semantic grammar and lowering

Only these child relationships are legal:

```
Defaults -> ConfigMap | Deployment | Service
ConfigMap -> Data
Deployment -> Container
Container -> Port | Env | Resources
Service -> Port
```

`Event` is action-result-only. `null`, `false`, and `undefined` omit a child
inside a semantic position. They do not authorize an empty graph: only
`explicitEmpty()` does. Managed IDs map only to ABI `logicalId`; physical
identity is exact GVK/name and namespace inherits from the root. Resource order
is `(apiVersion, kind, namespace, name, logicalId)`; Container, Port, Env, and
Service Port arrays preserve authored order and reject duplicate semantic names.

The canonical lowering has IDs `config`, `deployment`, and `service`; root
`portal` produces ConfigMap `portal-config`, Deployment `portal`, and Service
`portal-service`. Labels are `{ app: "portal" }`. The Deployment template
places the Defaults `podSecurityContext` at `spec.template.spec.securityContext`;
the ConfigMap value comes from `settings.current?.data?.message`; the Container
environment references that ConfigMap key; and the Service target port is the
named `"http"` port. Exact output lives in `semantic-lowering.json`.

## `oxideloop-json/v1`

The v1 DSL normalizes configuration and semantic props in the existing
renderer-v0 JSON domain. It accepts plain or null-prototype objects, dense
arrays, strings without unpaired surrogates, finite binary64 numbers other than
negative zero, and safe integral numbers. It rejects accessors, symbols, extra
array properties, cycles, sparse arrays, non-finite numbers, negative zero,
and unsafe integers. Limits are depth 64, a 256 KiB encoded string or key, and
a 2 MiB canonical document.

Object keys order by UTF-16 code units; arrays keep semantic order. The SDK
snapshot is an immutable input/equality aid only. Rust `canonical_bytes` is the
sole authority for ABI bytes and wire hashes. This policy has no wire field;
an incompatible change requires a DSL/compiler major or renderer ABI change.

## Deployment ownership projection

Generated Deployments are limited to the exact v1 shape captured in the golden
fixture. Ownership comparison ignores status, server metadata, documented API
defaults, and controller revision annotations while comparing every DSL-owned
field. A same-UID, exact-owner Deployment whose projection matches may refresh
its durable resourceVersion without an Apply. Projection differences use a
fresh conditional Apply; an owner, UID, or proof mismatch is never adopted.
