# CloudNativePG and PostgREST example

This example composes a CloudNativePG `Cluster`, a PostgREST `Deployment`, its
`Service` and a managed ConfigMap in one Apsides controller. The PostgREST
container reads public settings from ConfigMaps and receives its database URI
through a `secretKeyRef` to CloudNativePG's `database-app` Secret. Secret values
do not enter the TypeScript build or generated IR.

Changes to the managed API ConfigMap, the observed `api-settings` ConfigMap or
the observed database Secret declare a rollout of PostgREST. The declarations
do not claim that those reactions or drift correction have run in a live
cluster.

From the repository root, build the CLI and regenerate the typed component from
the pinned CloudNativePG 1.30.0 CRD:

```sh
cuenv task build

target/debug/aps crds fetch \
  --url https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml \
  --kind Cluster \
  --version v1 \
  --output examples/cnpg/.aps/crds/cnpg.ts
```

The `.aps/` directory is generated and ignored. You can also regenerate the
example types with `cuenv task cnpgTypes`. Refresh the explicit source
lock whenever `program.tsx` or the generated CRD module changes:

```sh
target/debug/aps lock \
  --source examples/cnpg/program.tsx \
  --modules examples/cnpg/.aps/crds/cnpg.ts
```

Typecheck and compile the example:

```sh
deno check --config examples/cnpg/deno.json examples/cnpg/program.tsx

target/debug/aps compile \
  --source examples/cnpg/program.tsx \
  --output work/cnpg-compiled
```

The compile output directory must be fresh. These commands use compiler assets
and installed JavaScript dependencies from this source checkout; the current CLI
is not a standalone installed distribution.

Compilation proves the locked source can produce validated IR and a controller
project. It does not install the CloudNativePG operator or CRD, create the
observed ConfigMap, deploy PostgREST, or prove database readiness, rollouts and
drift correction. Those require a separately prepared cluster and live
verification.

For the first deployment in the upgrade demo, use [the initial version](initial/README.md).
It owns the same database and ConfigMap; this version adds the API workload and
its observed inputs.
