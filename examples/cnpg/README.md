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

## Local upgrade rehearsal

The repository's test driver accepts two signed, integrity-verified candidates
built from the initial and parent examples. It uses the real lifecycle protocol
on a new kind cluster, including the rejected permission expansion and verified
writer retirement. It does not bypass the production `aps deploy` release gate
or produce a production acceptance receipt.

```sh
cuenv exec -- python3 tests/reakt/run_cnpg_demo.py \
  --initial-artifact "$INITIAL_CANDIDATE" \
  --successor-artifact "$SUCCESSOR_CANDIDATE" \
  --trust-policy "$TRUST_POLICY" \
  --operator-manifest "$PINNED_OPERATOR_MANIFEST" \
  --operator-sha256 "$OPERATOR_MANIFEST_DIGEST" \
  --output "$PWD/work/cnpg-rehearsal"
```

Input paths must be absolute and the output directory must be new. The operator
manifest must be CNPG 1.30.0 with its operator image and `OPERATOR_IMAGE_NAME`
pinned to the same digest. The driver runs the existing Envoy canary first;
its environment evidence supplements the upgrade's separate live drain,
termination and credential-revocation checks.

The HTTP checks expect the synthetic `messages` table. After the upgrade, a
ConfigMap change sets `PGRST_DB_MAX_ROWS` from 100 to 1; the checks require a new
Pod and ReplicaSet, a changed rollout revision and an HTTP response with one row
instead of two. Database access verifies consumption of the CNPG Secret; this
flow does not claim to demonstrate credential rotation.

By default the driver deletes its cluster and credentials, preserving existing
clusters. `--retain-on-success` keeps the successful demo cluster and a private
admin kubeconfig in the output directory, while removing issuer credentials.
Failure is never treated as successful release evidence. Offline runner
regressions are available through `cuenv task cnpgDemoTest`.
