---
title: Custom resources from CRDs
description: Generate typed Apsides components from a pinned namespaced Kubernetes CRD.
---

This workflow generates a typed `Cluster` component from the CloudNativePG
1.30.0 CRD, records it in the explicit source closure and compiles it with the
normal TypeScript authoring path. It does not install CloudNativePG or deploy a
workload.

## Create the project

Run these commands from the Apsides repository root after installing its pnpm
dependencies. The locally built CLI uses the checkout assets. With an installed
CLI bundle, use `aps` in place of `target/debug/aps`; compiler and CRD tooling
resolve from that installation.

```sh
cuenv task build
target/debug/aps init --directory work/cnpg-authoring
```

`aps init` creates `program.tsx`, editor configuration, a source lock and an
embedded SDK under `.aps/sdk/`. The generated `.aps/` directory is ignored by
Git.

## Generate the typed component

Fetch the CRD from the versioned `v1.30.0` tag and select its served `v1`
schema:

```sh
target/debug/aps crds fetch \
  --url https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml \
  --kind Cluster \
  --version v1 \
  --output work/cnpg-authoring/.aps/crds/cnpg.ts
```

The generated module exports `ClusterSpec` for editor completion and a `Cluster`
JSX component. Its descriptor fixes the API version, kind, namespaced REST
plural and a digest of the fetched CRD source. That digest binds the generated
module to those source bytes; it is not a canonical schema digest or proof of
the schema installed in a cluster.

Import the component and supply only object metadata, lifecycle policy and its
typed spec:

```tsx
import { Cluster } from "./.aps/crds/cnpg.ts";

<Cluster
  metadata={{ name: "database", namespace: "default" }}
  deletionPolicy="orphan"
  spec={{
    instances: 1,
    storage: { size: "1Gi" },
  }}
/>
```

The factory owns `apiVersion`, `kind` and REST routing metadata; application JSX
cannot replace them. A later Kubernetes server-side apply validates the object
against the CRD actually installed in the target cluster.

The compiler reports `opaque-custom-resource-effects` for imported resources.
The platform policy must explicitly accept this risk: a schema describes the
object, but cannot bound the actions its operator performs. For this example,
CloudNativePG must be installed separately with its own permissions; it creates
the PostgreSQL workloads, storage and connection Secret.

## Lock and compile

For the complete working example, replace the scaffold program with the checked
example, then refresh the source lock with the generated module explicitly:

```sh
cp examples/cnpg/program.tsx work/cnpg-authoring/program.tsx

target/debug/aps lock \
  --source work/cnpg-authoring/program.tsx \
  --modules work/cnpg-authoring/.aps/crds/cnpg.ts

target/debug/aps compile \
  --source work/cnpg-authoring/program.tsx \
  --output work/cnpg-compiled
```

Use a fresh compile output directory. A successful run establishes that the
locked TypeScript and generated component lower to validated Apsides IR in the
current checkout. It does not establish CRD installation, release trust,
deployment authorization, database readiness, reactive rollouts or drift
correction in a live cluster.

See the [CloudNativePG example](https://github.com/rawkode/apsides/tree/main/examples/cnpg)
for the PostgREST workload and its ConfigMap and Secret-driven rollout inputs.
