# Apsides TSX example

`program.tsx` declares the controller boundary and composes the reusable `Api`
component from `api.tsx`. The component declares Kubernetes resources with JSX
and observations with `useConfigMap` and `useSecret`. `rolloutOn` connects opaque
input projections to a Deployment; it never reads live values during the build.
The workload is a pinned, unprivileged NGINX image serving HTTP on port 8080.
Its Pods do not mount Kubernetes API credentials.

Compile the digest-locked example from the repository root:

```sh
target/debug/aps compile \
  --source examples/web-application/program.tsx \
  --output work/apsides-compiled
```

Choose a fresh output directory for each build. The compiler supplies source
locations and provenance, evaluates the component tree in its isolated build
process, and emits data for a native Kubernetes controller. No React or
TypeScript runtime is deployed into the cluster.

After editing either source file, refresh the explicit source lock:

```sh
target/debug/aps lock \
  --source examples/web-application/program.tsx \
  --modules examples/web-application/api.tsx
```

The source build performs a Deno type check, two Oxc/Rolldown builds and two
permission-restricted Deno/SES evaluations. It generates production IR,
separate test fixtures, source maps and the toolchain receipt. Generated files
stay in ignored output directories. Fixtures never enter production IR.

Resource locations identify their JSX declarations. Observation hooks currently
identify the containing component's JSX invocation, rather than the hook's
individual line. These locations are diagnostics, not authorization.

In a dedicated demo cluster, create the two external inputs in the workload
namespace before installing the controller. These values are demo data:

```sh
kubectl -n production create configmap external-settings --from-literal=THEME=blue
kubectl -n production create secret generic database-credentials --from-literal=PASSWORD=demo-only
```

After installation, check the workload and forward its HTTP service:

```sh
kubectl -n production rollout status deployment/api --timeout=120s
kubectl -n production port-forward service/api 8080:80
# In another terminal:
curl --fail http://localhost:8080/
```

Watch Pods while changing the observed ConfigMap:

```sh
kubectl -n production get pods -l app=api --watch
# In another terminal:
kubectl -n production patch configmap external-settings --type=merge \
  -p '{"data":{"THEME":"green"}}'
```

The controller changes the Deployment's Pod template revision, causing a rollout.
The page remains the NGINX welcome page: this example demonstrates reconciliation
and rollout on input changes, rather than an application that renders those inputs.
Secret changes drive the same behavior without printing their values.

Compilation and workload-image checks do not establish a deployed controller or
a production-qualified release. The live demo must also rehearse controller
installation and the observed rollout.
