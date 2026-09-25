---
title: Event handlers
description: React to a ConfigMap change with a sandboxed TypeScript handler that fetches from declared HTTPS hosts and writes a handler output.
---

An event handler is a TypeScript function the controller runs when one of its
declared inputs changes. It can call declared HTTPS hosts and write keys into
its own output `ConfigMap`, and a Deployment can roll out when that output
changes. Everything it can read, reach and write is fixed at build time, is
visible in `surface.json` and `aps diff`, and is bound to the deployment
review.

## When to use a handler

Use a rollout reaction (`rolloutOn`) when a workload only needs to restart
when configuration it already reads changes. Use a handler when the controller
has to compute or fetch something first: for example, resolve feature flags
from an HTTPS service whenever a settings `ConfigMap` changes, and hand the
result to a workload as a `ConfigMap`.

A handler is not a general workflow engine. It cannot call the Kubernetes API,
read Secrets, create objects other than its declared outputs, run on a
schedule or react to anything but a declared input's data changing.

## Authoring

Declare the output with `useHandlerOutput` and the handler with `<On>`:

```tsx
import {
  Controller, Deployment, On, useConfigMap, useHandlerOutput,
} from "@apsides/kubernetes";

const Api = () => {
  const settings = useConfigMap({ id: "feature-settings", name: "feature-settings" });
  const flags = useHandlerOutput({ id: "resolved-flags", name: "resolved-flags" });

  return <>
    <On
      id="resolve-flags"
      trigger={settings.changed}
      handler="./handlers/resolve-flags.ts#resolveFlags"
      inputs={[settings]}
      egress={["flags.example.com"]}
      outputs={[flags]}
      timeoutMillis={10000}
    />
    <Deployment
      metadata={{ name: "api", namespace: "production" }}
      rolloutOn={[flags.data]}
      spec={{ /* ... */ }}
    />
  </>;
};
```

| Prop | Meaning |
| --- | --- |
| `trigger` | The `.changed` event of a `useConfigMap` observation or a `useResourceRef("ConfigMap")` reference. It must also appear in `inputs`. |
| `handler` | `"<module path>#<export name>"`. The path is resolved from the directory holding `apsides.lock.json`, not from the file containing `<On>`. |
| `inputs` | Nonempty list of non-sensitive `ConfigMap` references. `useSecret` observations are rejected. |
| `egress` | Hosts the handler may call over HTTPS, as `"host"` (port 443) or `"host:port"`. |
| `outputs` | References returned by `useHandlerOutput`. Each output has exactly one writer handler. |
| `id` | Optional stable handler id. Without it the compiler derives one from the export and module path. |
| `timeoutMillis`, `maxHeapBytes` | Optional limits, at most 30000 ms and 64 MiB; both default to the maximum. |

`useHandlerOutput({ id?, name, namespace? })` declares a managed `ConfigMap`
in the controller's namespace. The controller creates it with only its
identity and the reserved `apsides.dev/handler-output` label; only the handler
writes its `data`. Its `.data` projection works in `rolloutOn` like any other
input projection.

### Handler modules

- `handler` must be a string module reference. Passing a function fails
  compilation: handlers run in a separate sandbox and cannot capture
  build-time closures.
- The module must be in the source lock. Lock it with the entry program, for
  example `aps lock --source program.tsx --modules handlers/resolve-flags.ts`.
  Locked modules must live under the entry program's directory.
- A handler module may import only other locked modules, through relative
  specifiers. npm, `jsr:`, remote and SDK imports and dynamic `import()` fail
  compilation.
- The compiler bundles each handler into one closed ES module, twice, and
  requires byte-identical output. The bundle's SHA-256 is recorded in the IR
  and the bundle is embedded in the controller binary.
- `aps compile` bundles handler modules but does not type-check them, and the
  SDK ships no type declaration for the `apsides` global.

Validation also rejects a handler that triggers on or reads its own output, and
any cycle in which handlers feed each other through their outputs. The
[IR contract](/reference/ir-contract/#handlers-and-outputs) lists every rule.

### Egress hosts

Each egress host must be a lowercase DNS name with at least one dot. IP
addresses, numeric shorthands such as `2130706433`, `localhost`, names under
`.localhost`, single-label names and wildcards are rejected. A domain may
declare at most 16 handlers, 16 distinct egress hosts and 256
`(host, port, handler)` egress rows.

## The handler API

The export is called with one event argument and may be `async`:

```ts
export async function resolveFlags(event: {
  id: string;
  trigger: { kind: "input-changed"; input: string };
  inputs: Record<string, { data: Record<string, string> }>;
}) {
  const environment = event.inputs["feature-settings"].data.environment ?? "production";
  const response = await apsides.fetch(
    `https://flags.example.com/v1/flags?env=${environment}`,
    { headers: { accept: "application/json" } },
  );
  if (response.status !== 200) throw new Error(`flag service returned ${response.status}`);
  apsides.writeOutput("resolved-flags", "flags.json", response.body);
  apsides.log("info", { status: response.status });
}
```

`event.inputs` holds the current `data` of every declared input, keyed by
input id. `event.id` is a digest of the handler id and the input revision; it
stays the same when the same revision is delivered again, so use it as an
idempotency key.

The handler's host API is one frozen global, `apsides`. The embedding's own
`Deno` object and bootstrap internals are deleted before the module runs, no
Node or Deno runtime extensions are registered (so there is no `TextEncoder`),
there is no filesystem and no environment, and the handler holds no Kubernetes
credential.

### `apsides.fetch(url, { method?, headers?, body? })`

Returns `{ status, headers, body }`: `headers` is a list of `[name, value]`
pairs and `body` a `Uint8Array`. A failed call throws an `Error` whose `code`
names the reason, such as `HOST_NOT_ALLOWED`, `SCHEME_NOT_HTTPS`,
`NO_GLOBAL_UNICAST_ADDRESS`, `RESPONSE_TOO_LARGE` or `TIMEOUT`.

- The URL must be `https`, its host must not be an IP literal, and its host
  and port must match one of the handler's own egress entries exactly.
- The host is resolved with the controller's own DNS resolver, and the call
  connects only to resolved addresses that are global unicast. Loopback,
  private, link-local, carrier-grade NAT, documentation, benchmarking,
  multicast, reserved and unique-local addresses are skipped, as are
  non-global IPv4 addresses embedded in IPv4-mapped, NAT64, 6to4 and Teredo
  IPv6 forms. If none remains, the call fails with
  `NO_GLOBAL_UNICAST_ADDRESS`.
- TLS verifies the server against the bundled public WebPKI roots with SNI set
  to the declared host. No client certificate, OS trust store or proxy
  settings are used.
- Redirects are returned as responses, never followed.
- `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `TE`,
  `Upgrade` and `Trailer` request headers are refused. `method` defaults to
  `GET`; `body` is a string or `Uint8Array`.
- Requests use HTTP/1.1 and carry no credentials unless the handler adds a
  header itself. Handlers cannot read Secrets, so there is no supported way to
  supply one.

### `apsides.writeOutput(outputId, key, data)`

Writes one key of a declared output. `key` must match `[-._a-zA-Z0-9]+`, be at
most 253 characters and not be `.` or `..`. `data` is a string or a
`Uint8Array` of valid UTF-8 text; it becomes a `ConfigMap` `data` value. Writes
are buffered and committed only after the handler returns successfully. Errors
carry a `code` such as `UNDECLARED_OUTPUT_ID`, `INVALID_KEY` or
`DATA_TOO_LARGE`.

### `apsides.log(level, message)`

`level` is `debug`, `info`, `warn` or `error`; a non-string message is
JSON-encoded. Each line is written to the controller's stderr as JSON tagged
with the handler and invocation ids.

### Limits

| Limit | Value |
| --- | --- |
| Wall-clock time per invocation | `timeoutMillis`, at most 30 s, enforced by a watchdog even against code that never yields; `fetch` deadlines never outlive it |
| V8 heap | `maxHeapBytes`, at most 64 MiB; reaching it terminates the isolate |
| `ArrayBuffer` memory | half the heap, at most 32 MiB; resizable buffers are disabled |
| `fetch` request body | 1 MiB per call |
| `fetch` response body | 4 MiB per call, and 16 MiB across the whole invocation |
| `fetch` response headers | 100 headers, 64 KiB |
| Concurrent `fetch` calls | 4 |
| One output's written keys and values | the domain's `maxObjectBytes`, per invocation |
| Log lines | 64 per invocation, 4 KiB each; extra lines are dropped and counted |

Every invocation runs in a fresh V8 isolate with JIT compilation disabled,
and without WebAssembly or `SharedArrayBuffer`. The controller Pod's memory
limit grows with the domain's largest `maxHeapBytes` plus these per-invocation
budgets.

## Delivery

- **Trigger.** Each reconcile cycle computes a revision for every handler from
  its id, bundle digest, export, outputs, trigger and the data of every
  declared input. The handler is queued when that revision differs from the
  one recorded on its outputs, or when an output was deleted, recreated or
  lost its annotation. A handler whose inputs are not all available is not
  queued.
- **Coalescing.** A queued handler whose revision changes again is replaced by
  the latest event. Intermediate values are not delivered.
- **Dispatch.** At most one invocation starts per reconcile cycle, and
  handlers never run concurrently. A handler waits until all its outputs were
  applied in that cycle; otherwise the controller reports
  `handler-output-not-ready`. The queue holds at most `maxQueueDepth`
  handlers; one more reports `handler-queue-overflow`.
- **Commit.** On success, each declared output is committed with one
  server-side apply under field manager `apsides-handler-<domainId>`, with the
  `apsides.dev/handler-input-revision` annotation. The keys written in that
  invocation replace the handler's previous keys; keys other managers own are
  never touched; an output the invocation did not write keeps the handler's
  existing keys.
- **Failure.** A throw, timeout or limit breach commits nothing. A commit
  failure stops at the failing output: outputs committed earlier in the same
  attempt keep their new keys and revision, so a workload that rolls out on
  one of them can see the partial result. Either way the controller reports
  `handler-invocation-failed:<CODE>` and retries the same event with
  exponential backoff, and the handler stays pending until every output
  records the same revision.
- **Restarts.** After a restart the controller reads the recorded revision
  from the outputs and runs a handler only if its revision differs, or if its
  outputs disagree. A handler that declares no outputs has nothing to record a
  revision on, so it runs again after every restart.

Delivery is at least once. A handler may run again for a revision it already
processed, for example after a crash between its fetch and the commit, so it
should be idempotent and use `event.id` when it calls an external system that
has side effects.

The controller's `/status` lists each handler's last run, last error code and
whether it is queued. Handler conditions appear with the controller's other
conditions.

## Security model

- **Egress allowlist.** The allowlist is the handler's own declared
  `(host, port)` pairs, compiled into the binary. A handler cannot reach
  another handler's hosts, an IP literal or a non-HTTPS URL.
- **Address vetting.** DNS answers are checked before connecting, so a declared
  name that resolves to a cluster, node or metadata address is refused.
- **No filesystem.** The handler's module loader serves only its embedded
  bundle, and the isolate has no file, process or environment operations.
- **Landlock.** Before any thread starts, the controller reads every file it
  will need into memory and applies a deny-all Landlock filesystem ruleset.
  V8 and the async runtime start afterwards and inherit it. A kernel that
  cannot enforce Landlock stops the controller; only a fixture-mode
  configuration can opt out. `/status` reports `lockdown` as `landlock`,
  `landlock-partial` or `none-fixture`.
- **Integrity.** The controller refuses to start unless its embedded bundles
  match the `bundleSha256` values in its validated IR.
- **Separate ownership.** Handler writes use their own field manager and the
  same authority and claim checks as reconciler writes.

Egress is enforced by the controller, not by the cluster network: Apsides
does not generate a `NetworkPolicy` for handler egress.

## Review

`aps compile` writes `surface.json`. Its `handlers` section lists each handler's
id, trigger, inputs and outputs; `egress` lists each `(host, port, handler)`;
and `outputs` lists each output's id, name and namespace. Handler bundle
contents never appear in it.

In a pull request, `aps diff` reports added, removed and changed handlers and
outputs, and added and removed egress rows. The `egress.added` and `handlers.added` policy rules
require approval for new egress or new handlers; see
[Surface diff in CI](/guides/surface-diff/). At deploy time the review lists the
same sections next to permissions and Secret reads, and `aps deploy` refuses a
release whose surface digest differs from the reviewed one; see
[Build and deploy](/guides/build-and-deploy/).

## Current limitations

- `input-changed` on non-sensitive `ConfigMap` inputs is the only trigger.
- Handlers cannot read Secrets, so `fetch` requests carry no managed
  credentials.
- No `NetworkPolicy` is generated for egress.
- Fixture-mode controllers, and therefore `aps test --cluster`, verify handler
  bundles but do not execute handlers.
- Handler modules are not type-checked.
- Live acceptance of handlers runs only on the pinned Kubernetes v1.34.3 kind
  node, with a handler that makes no `fetch` calls.

A runnable example of the flow above — a `feature-settings` input, a handler
that fetches from `https://flags.example.com`, a handler output and a
Deployment that rolls out on it — is in `examples/events/` in the repository.
