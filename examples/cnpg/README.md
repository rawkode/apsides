# CloudNativePG and PostgREST demo

Start with a database, add an API that retains its database declaration, then
change live inputs and watch the API react. [The initial program](initial/program.tsx)
owns the CNPG `Cluster` and API ConfigMap; [the upgraded program](program.tsx)
retains them and adds PostgREST, its Service and observed inputs.

The API consumes ConfigMaps and CNPG's `database-app` Secret in the cluster.
Its database URI uses `secretKeyRef`; Secret values never enter the authored
program or compiled IR.

## 1. Author with imported CNPG types

Run from the repository root, using a new project directory:

```sh
cuenv task build
target/debug/aps init --directory work/cnpg-authoring

target/debug/aps crds fetch \
  --url https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml \
  --kind Cluster \
  --version v1 \
  --output work/cnpg-authoring/.aps/crds/cnpg.ts
```

Open `work/cnpg-authoring` in your editor. The scaffold includes `deno.json`,
`tsconfig.json`, a source lock and the local SDK. For VS Code with Deno, install
the Deno extension and enable it for this workspace.

Import `Cluster` into `program.tsx` and demonstrate completion inside `spec`
and `storage`:

```tsx
import { Cluster } from "./.aps/crds/cnpg.ts";

<Cluster
  metadata={{ name: "database", namespace: "default" }}
  deletionPolicy="orphan"
  spec={{ instances: 1, storage: { size: "1Gi" } }}
/>
```

The module also exports `ClusterSpec`. Hover descriptions come from the CRD;
misspelled fields and incorrect types produce editor diagnostics.
`cuenv task authoringEditorCheck` tests this through the actual Deno language
server. See [custom resource authoring](../../docs/guides/custom-resources.md)
for the full workflow. Generated `.aps/` files stay ignored.

To compile the complete checked-in API example:

```sh
cuenv task cnpgTypes

target/debug/aps lock \
  --source examples/cnpg/program.tsx \
  --modules examples/cnpg/.aps/crds/cnpg.ts

deno check --config examples/cnpg/deno.json examples/cnpg/program.tsx

target/debug/aps compile \
  --source examples/cnpg/program.tsx \
  --output work/cnpg-compiled
```

Use a fresh compile output directory. Refresh the lock after changing source
or generated types. [Initial-version instructions](initial/README.md) cover the
database-only program. An installed [CLI bundle](../../docs/reference/CLI-DISTRIBUTION.md)
provides these commands as `aps`.

## 2. Deploy the database, then upgrade to the API

The acceptance suite's CNPG scenario runs the public `aps deploy` command for
both versions on a new kind cluster, then checks workload behavior. It builds
and signs both releases, installs CNPG and provisions the platform trust,
permissions and identities itself; compilation alone does not prepare these
deployment inputs. See the [acceptance suite](../../docs/reference/ACCEPTANCE.md)
for its prerequisites, and [build and deployment inputs](../../docs/guides/build-and-deploy.md)
for what a real platform must provide.

```sh
cuenv task acceptancePrepare
cuenv task acceptanceTest
```

Presentation checkpoints:

1. **Database ready:** the initial controller creates the database and seeds two
   `messages` rows. There is no API Deployment yet.
2. **API upgrade:** under the database-only policy `aps deploy` refuses the API
   version before creating anything. The scenario then widens the approved
   permission policy and `aps deploy` upgrades to the API version. The database
   declaration stays unchanged; PostgREST serves the two seeded rows using the
   CNPG Secret.
3. **ConfigMap reaction:** `api-settings` changes `PGRST_DB_MAX_ROWS` from `100`
   to `1`. The API gets a new Pod, ReplicaSet and rollout revision while its
   Deployment UID remains stable. HTTP now returns one row: the response limit
   changed; this is not a database-content mutation.
4. **Equal data:** a metadata-only ConfigMap update leaves the API workload
   unchanged for a 30-second observation window.
5. **Secret reaction:** adding an unused synthetic key to `database-app` triggers
   another rollout with working database access. This demonstrates a Secret-data
   reaction, not password rotation. The probe removes only its own key, checks
   that the original Secret data is restored in memory and waits for the restored
   workload. Secret values are not saved in evidence.

The scenario also inserts a public row in a separate `aps_demo_persistence`
table before upgrading, then requires that row and the same CNPG resource and
PVC identities afterward. This leaves the visible `messages` demo unchanged.

The example uses `deletionPolicy="orphan"` for its resources. That protects the
intended resource lifecycle; it is not a database backup. The scenario deletes
its disposable cluster, including its demo data, and its credentials.

Read `work/acceptance/results/cnpg-upgrade/report.json` for every check and the
CLI commit reports. The `aps` output and controller logs sit beside it, with
Secret values, the rollout key and tokens redacted.
