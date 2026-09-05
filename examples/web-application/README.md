# WebApplication example

The application controller is authored in React-style TypeScript at
[`packages/example-bundle/src/controller.tsx`](../../packages/example-bundle/src/controller.tsx).
It declares the nginx Deployment and Service, renders a ConfigMap, watches a
second ConfigMap with `useResource`, enriches every object, and runs an
event-driven automation when the watched data changes.

`oxideloop build` compiles that TSX to OxideLoop's deterministic intermediate
representation, embeds the IR archive in a Rust controller image, saves that
image locally, and writes a build receipt. `oxideloop deploy` creates or reuses
the explicitly named kind cluster, loads the exact saved image, installs the
CRDs and namespaced controller, creates the root `WebApplication`, and waits
for the generated workload to be ready. The cluster does not contain a bundle
ConfigMap; the controller image is the program artifact.

## Run the example

From the repository root, with Docker running and `kind`, `kubectl`, `pnpm`,
and the Rust toolchain installed:

```sh
cargo build --locked -p legacy-cli

target/debug/oxideloop build \
  --source packages/example-bundle/src \
  --image oxideloop-example-controller \
  --output work/example

target/debug/oxideloop deploy \
  --build work/example/receipt.json \
  --kind-cluster oxideloop-example \
  --namespace oxideloop-example \
  --root-name root \
  --workload-image nginxinc/nginx-unprivileged:alpine
```

The short version runs those same commands and prints the resulting resources:

```sh
make example
```

In another terminal, expose the generated web Service:

```sh
make example-forward
```

Then open <http://localhost:8080>. The page is served by the workload declared
by `controller.tsx`, not by the OxideLoop controller pod.

## Exercise the hooks

Create the ConfigMap watched by `useResource` and wait for the rendered output
to change:

```sh
kubectl --context kind-oxideloop-example --namespace oxideloop-example \
  create configmap demo-input --from-literal=message=vanilla

kubectl --context kind-oxideloop-example --namespace oxideloop-example \
  wait --for='jsonpath={.data.message}=vanilla' configmap/root-config \
  --timeout=180s

STATE=$(kubectl --context kind-oxideloop-example --namespace oxideloop-example \
  get webapplication/root -o jsonpath='{.status.state.name}')

kubectl --context kind-oxideloop-example --namespace oxideloop-example \
  wait --for='jsonpath={.spec.state.automation.baselines[0].automationId}=configuration-changed' \
  "oxideloopstate/$STATE" --timeout=180s
```

Change the watched value:

```sh
kubectl --context kind-oxideloop-example --namespace oxideloop-example \
  patch configmap/demo-input --type=merge \
  --patch='{"data":{"message":"chocolate"}}'

kubectl --context kind-oxideloop-example --namespace oxideloop-example \
  wait --for='jsonpath={.data.message}=chocolate' configmap/root-config \
  --timeout=180s

make example-status
```

The status output includes the rendered message, successful `AutomationRun`,
and `ConfigurationChanged` Kubernetes Event.

Re-running `oxideloop deploy` with the same receipt is an exact, write-free
resume. It refuses fixed-name objects that do not match that receipt instead of
adopting or overwriting them.

Remove the disposable cluster when finished:

```sh
make example-clean
```

For the exhaustive crash, recovery, pruning, and ownership suite, use
`scripts/run-kind-e2e.sh`.
