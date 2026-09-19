# Reactive TSX Kubernetes controller

A component-authored controller using the **real Alchemy deployment engine and Kubernetes API**. `useResource` observes an unmanaged ConfigMap, waits internally until it exists, and rerenders the application when it changes. `envFrom` generates the Kubernetes reference and a Pod-template checksum so changed environment data rolls the Deployment.

Unmanaged ConfigMaps are **read-only**: the controller never creates, patches or deletes them. `useFinalizer` registers deletion cleanup on a managed resource ref.

## Application

See [examples/website.tsx](examples/website.tsx). There is no status guard in the component:

```tsx
function Website() {
  const config = useResource(ConfigMap, { name: "nginx-config" });
  const nginx = useResourceRef();

  useFinalizer(nginx, "website.example.com/cleanup", async ({ resource, signal }) => {
    signal.throwIfAborted();
    console.log("Finalizing", resource.id);
  });

  return <Deployment
    id="nginx" name="nginx" image="nginx:1.27-alpine"
    replicas={2} port={80} ref={nginx} envFrom={[config]}
  >
    <Service id="service" readyAfter={[nginx]} />
  </Deployment>;
}
```

The application component runs beneath `<Controller name="website" namespace="tsx-e2e">`, allowing the hook to inherit the namespace. Nested Service inherits the Deployment's name, namespace, selector and single port. Readiness dependencies remain explicit through `readyAfter`.

## Run the live local test

With Bun, Docker, kind and kubectl installed, and Docker running:

```sh
cd prototypes/tsx-controller
bun install --frozen-lockfile
bun run check
bun test
bun run e2e
```

The test creates a fresh isolated kind cluster named `tsx-reactive`, builds a Linux executable image and installs the generated Role. It uses a dedicated kubeconfig under `.e2e/` and preserves your selected context. It refuses to overwrite existing clusters; choose another name for subsequent runs:

```sh
TSX_E2E_CLUSTER=tsx-reactive-next bun run e2e
```

The live test checks:

1. Starting without `nginx-config` creates no managed workloads.
2. Creating the ConfigMap allows nginx and its Service to become ready; a separate Pod reaches nginx through Service DNS and ClusterIP.
3. Changing ConfigMap data creates replacement Pods whose actual environment contains the new value.
4. Metadata-only ConfigMap edits leave the Pod template and Pod identities unchanged.
5. Deleting the ConfigMap preserves the existing managed resources, including across controller Pod replacement.
6. Recreating the ConfigMap with a new UID and changed data resumes reconciliation and rolls new Pods.
7. Desired replica updates, drift repair and persistent-state restart recovery still work.
8. The generated Role permits named ConfigMap get/list/watch; real unauthorized create/patch/delete requests are denied.
9. Managed-resource finalization runs the hook and deletes nginx/Service while the unmanaged ConfigMap survives with the same UID and data.

The cluster and `.e2e/<run>/` remain for inspection. Evidence contains a source SHA256 manifest, Kubernetes/image details, Pod/configuration checks, plan and controller/finalizer logs. The script rejects source changes during acceptance. It ends with managed workloads cleaned up and the controller scaled to zero.

Use the kubeconfig printed by the test to inspect its cluster. Remove it when finished:

```sh
kind delete cluster --name tsx-reactive
```

`cuenv task tsxPrototypeE2e` is configured at the repository root. Repository CUE evaluation previously hit its 10-second timeout; the direct Bun commands are the verified path.

## Reactive behavior

The runtime reads required ConfigMaps, renders from those snapshots and reconciles the resulting graph. Named Kubernetes watches resume from the observed resource version and wake reconciliation. A bounded refresh also covers dropped watch connections and expired versions.

A missing or deleting required resource pauses desired-resource changes for the application. Existing resources are preserved, not pruned or recreated from a placeholder render. Already-requested deletion can still progress. Recreating an unmanaged ConfigMap is observed normally even though its UID changes; observation never grants ownership.

`envFrom` hashes sorted ConfigMap `data`, including the reference identity, into a reserved Pod-template annotation. Metadata, resource version, UID and `binaryData` do not affect it. This matches the environment source: binaryData is not injected by ConfigMap envFrom. Metadata-only changes may rerender the component but do not change the desired Pod template. A changed hash triggers a normal Kubernetes Deployment rollout through a [Pod-template update](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/#updating-a-deployment).

The checksum and reference provide eventual convergence, not an atomic snapshot of an externally mutable ConfigMap at every Pod start. Another external change during a rollout causes subsequent reconciliation to roll the latest configuration.

## Finalizers and permissions

`useFinalizer(ref, qualifiedKey, async callback)` attaches durable cleanup to a managed resource. It runs on resource deletion, not React-style unmount or dependency changes. It removes its own finalizer only after success and preserves foreign/system finalizers. Cleanup is at least once and must be idempotent. Errors/timeouts retain the obligation; callbacks should honor their AbortSignal.

Last successful render snapshots are stored before reconciliation under the same writer lock. After restart, cleanup can reconstruct callbacks using real saved data even if a required ConfigMap is missing. These are the most recent saved render inputs, not immutable per-resource creation inputs. Missing handler code blocks deletion; retain the previous executable when upgrading code with outstanding finalizers.

`plan` derives workload permissions from managed declarations and read-only `useResource` observations. The live installer binds the generated Role to the controller ServiceAccount. There is no runtime self-escalation. Live renders that introduce unplanned observations, resource identities or expanded deletion authority are rejected before mutation; permission-bearing declarations should be unconditional and changed through deployment.

ConfigMap observation receives named get/list/watch only. Managed Deployment/Service creation requires namespace-wide create for those kinds because [Kubernetes RBAC cannot restrict create by resource name](https://kubernetes.io/docs/reference/access-authn-authz/rbac/). Other generated rules are named. There is no create/delete/patch ConfigMap grant in this example.

The authoring API rejects mutation access options on `useResource`. Arbitrary callback JavaScript can still use independently supplied SDKs or credentials; generated RBAC remains the cluster-side enforcement boundary.

## Executable and commands

```sh
bun run build
bun run smoke
./dist/controller plan --namespace tsx-e2e
```

The host executable embeds the application and Bun runtime. The packaging smoke copies it to an isolated directory and checks its plan. On macOS the build refreshes an ad-hoc signature; this is not distribution notarization. The Dockerfile compiles a Linux executable and copies it into a runtime image with no TypeScript source or separate Bun installation. Effect remains inside the Alchemy adapter.

Inside Kubernetes, the executable uses the service-account token and CA directly, refreshing the token per request. It does not invoke kubectl:

```sh
controller run --namespace tsx-e2e --state /state
controller reconcile --namespace tsx-e2e --state /state
controller status --namespace tsx-e2e --state /state
controller finalize --namespace tsx-e2e --state /state
```

`reconcile` performs one cycle; exit 2 means waiting or blocked. `finalize` uses saved render inputs and retries pending deletion to a deadline (`--timeout` milliseconds, default 120000). It does not apply the cached desired graph. A partial cleanup scope cannot leave managed dependents of selected resources outside the scope. The example accepts `NGINX_REPLICAS` and `NGINX_IMAGE` at startup.

## Boundaries

This is a custom React-style JSX/hook runtime, not React DOM or a multi-instance CRD operator. Required-input suspension currently covers the whole application graph. Live resource kinds are Deployment, Service and ConfigMap; reactive unmanaged hooks currently support ConfigMaps. Cloud-shaped graph/fault-test fixtures have no live cloud adapters.

The local controller uses a Recreate rollout and persistent state on one kind node. An OS-released SQLite lock serializes cooperating processes sharing that filesystem; it is not distributed HA or fencing. JSON snapshots use atomic replacement without multi-file transactions or power-loss guarantees. Saved snapshots include ConfigMap data; treat the state directory as confidential.

`src/engine` retains a deterministic simulated engine for failure-injection tests. The deployed executable uses `src/kubernetes` and real API calls. Local acceptance establishes the tested lifecycle on its recorded cluster; production storage, distributed leases, finalizer code migrations and cloud providers remain outside this prototype.
