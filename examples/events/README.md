# Event handler example

`program.tsx` declares the event plane end to end: an observed input, one
handler that reaches one external HTTPS host, the ConfigMap it writes, and a
Deployment that rolls out when that ConfigMap changes.

- **Input.** `useConfigMap` observes `feature-settings` in `default`. An
  operator creates and edits it; this domain never manages it. Its
  `environment` key names the flag set to fetch.
- **Handler.** `<On trigger={settings.changed} ...>` runs `syncFlags` from
  [`handlers/sync-flags.ts`](handlers/sync-flags.ts) each time
  `feature-settings` changes. It calls
  `https://flags.example.com/v1/environments/<environment>/flags`, expects
  `{"flags": {"<name>": true | false, ...}}`, and writes one string key per
  flag (`new-checkout` becomes `FEATURE_NEW_CHECKOUT=true`), plus
  `FLAGS_ENVIRONMENT`, with `apsides.writeOutput`.
- **Output.** `useHandlerOutput` declares `feature-flags`, a ConfigMap the
  domain creates and the handler fills. Each successful run replaces the
  handler's own keys in it, so a flag removed upstream disappears from it.
- **Rollout.** The `storefront` Deployment loads `feature-flags` with
  `envFrom` and lists `flags.data` in `rolloutOn`, so a change to the output
  changes its Pod template revision and rolls it out.

The handler throws on any failure: a missing or malformed `environment`, a
network or TLS error, a non-2xx answer, or a response that is not the
expected document. A failed invocation commits nothing, so the storefront
keeps the last flags fetched successfully while the runtime retries the same
event with backoff and reports a `handler-invocation-failed` condition.

## Declared capabilities

A handler gets only what its `<On>` element declares, and the compiler
records each capability in the artifact's `surface.json`:

| Capability | Declared as | What the controller enforces |
| --- | --- | --- |
| Inputs | `inputs={[settings]}` | The event carries only `feature-settings`' data. |
| Egress | `egress={["flags.example.com"]}` | `apsides.fetch` reaches only `https://flags.example.com:443`. A request to any other host, port or scheme fails before connecting (for example with `HOST_NOT_ALLOWED`). |
| Output | `outputs={[flags]}` | `apsides.writeOutput` writes only `feature-flags`, as UTF-8 string values within the domain's `maxObjectBytes`. |

The handler module has no filesystem access and no other network access. It
may import only other locked modules, never a package.

## Compile and read the surface

From the repository root, after `cuenv task build` and `cuenv task install`:

```sh
target/debug/aps compile \
  --source examples/events/program.tsx \
  --output work/events-compiled
jq '{handlers, egress, outputs}' work/events-compiled/surface.json
```

Choose a fresh output directory for each build. The surface lists the
handler, one egress row for each declared host, port and handler, and the
output:

```json
{
  "handlers": [
    { "id": "sync-flags", "inputs": ["feature-settings"], "outputs": ["feature-flags"], "trigger": "feature-settings" }
  ],
  "egress": [
    { "handler": "sync-flags", "host": "flags.example.com", "port": 443 }
  ],
  "outputs": [
    { "id": "feature-flags", "name": "feature-flags", "namespace": "default" }
  ]
}
```

The compiler bundles the handler into `controller/src/handlers/sync-flags.js`
and records its digest as the IR's `bundleSha256`. The generated controller
embeds that bundle and never evaluates the TSX program itself.

To review a change, compile the edited program into a second directory and
compare the two surfaces:

```sh
target/debug/aps diff --base work/events-compiled --head work/events-edited
```

Adding a second host to `egress`, for example, reports
`+ egress status.example.com:443 <- handler sync-flags`. With a `--policy` whose
`requireApproval` names `egress.added`, `aps diff` exits 1 to mark the change
as needing approval; `handlers.added` matches only a newly declared handler,
not new egress on an existing one (see the [surface diff guide](../../docs/guides/surface-diff.md)).

After editing either source file, refresh the source lock:

```sh
target/debug/aps lock \
  --source examples/events/program.tsx \
  --modules examples/events/handlers/sync-flags.ts
```

To type-check both files, including the handler, which the compiler bundles
without type-checking:

```sh
deno check --config examples/events/deno.json \
  examples/events/program.tsx examples/events/handlers/sync-flags.ts
```

## Where the handler runs

Only a production-mode controller runs the handler, in its embedded engine.
Fixture mode (`aps test --cluster`) verifies that the embedded bundle matches
the IR but never runs it. `flags.example.com` is a documentation domain: point
`egress` and `FLAG_SERVICE_ORIGIN` at a real flag service before deploying,
or every invocation fails and `feature-flags` never receives flag keys.

`cargo test --locked -p cli --test events_example` compiles this example,
checks the surface rows above, and runs the embedded bundle against a local
HTTPS flag service. It covers a successful run and the failure paths that
write nothing.
