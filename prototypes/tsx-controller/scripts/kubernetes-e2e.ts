import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Deliberately leaves the isolated cluster and evidence for inspection.
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const cluster = process.env.TSX_E2E_CLUSTER ?? "tsx-reactive";
const namespace = "tsx-e2e";
const directory = join(root, ".e2e", `${cluster}-${Date.now()}`);
const kubeconfig = join(directory, "kubeconfig");
const context = `kind-${cluster}`;
const image = `tsx-controller:e2e-${Date.now()}`;
const evidence: Record<string, unknown> = { cluster, context, namespace, directory, image };
mkdirSync(directory, { recursive: true });

function sourceManifest() {
  const files = ["Dockerfile", ".dockerignore", "package.json", "bun.lock", "tsconfig.json", "scripts/kubernetes-e2e.ts"];
  for (const folder of ["src", "examples"]) {
    for (const entry of readdirSync(join(root, folder), { recursive: true, withFileTypes: true })) {
      if (entry.isFile()) files.push(join(entry.parentPath, entry.name).slice(root.length + 1));
    }
  }
  return Object.fromEntries(files.sort().map(file => [file, createHash("sha256").update(readFileSync(join(root, file))).digest("hex")]));
}

async function command(args: string[], input?: unknown, allowFailure = false) {
  const child = Bun.spawn(args, {
    cwd: root,
    env: { ...process.env, BUILDX_CONFIG: join(directory, "buildx") },
    stdout: "pipe",
    stderr: "pipe",
    ...(input === undefined ? {} : { stdin: new Blob([JSON.stringify(input)]) }),
  });
  const deadline = setTimeout(() => child.kill("SIGKILL"), 600_000);
  let exitCode: number, stdout: string, stderr: string;
  try {
    [exitCode, stdout, stderr] = await Promise.all([
      child.exited, new Response(child.stdout).text(), new Response(child.stderr).text(),
    ]);
  } finally { clearTimeout(deadline); }
  if (exitCode && !allowFailure) throw new Error(`${args.join(" ")} failed (${exitCode})\n${stdout}\n${stderr}`);
  return { exitCode, stdout, stderr };
}

const kubectl = (args: string[], input?: unknown, allowFailure = false) =>
  command(["kubectl", "--kubeconfig", kubeconfig, "--context", context, "-n", namespace, ...args], input, allowFailure);
const apply = (items: unknown[]) => kubectl(["apply", "-f", "-"], { apiVersion: "v1", kind: "List", items });
async function get(kind: string, name: string) {
  const result = await kubectl(["get", kind, name, "-o", "json"], undefined, true);
  if (result.exitCode && result.stderr.includes("NotFound")) return undefined;
  if (result.exitCode) throw new Error(result.stderr);
  return JSON.parse(result.stdout);
}
async function waitFor(label: string, predicate: () => Promise<boolean>, timeout = 180_000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await predicate()) { console.log(`PASS ${label}`); return; }
    await Bun.sleep(1000);
  }
  throw new Error(`Timed out: ${label}`);
}
async function stableFor(label: string, predicate: () => Promise<boolean>, duration = 10_000) {
  const end = Date.now() + duration;
  do {
    assert(await predicate(), label);
    await Bun.sleep(1000);
  } while (Date.now() < end);
  console.log(`PASS ${label}`);
}
async function nginxPods() {
  return JSON.parse((await kubectl(["get", "pods", "-l", "app.kubernetes.io/name=nginx", "-o", "json"])).stdout).items;
}
async function podsHaveEnvironment(value: string, count: number) {
  const pods = await nginxPods();
  if (pods.length !== count || pods.some((pod: any) => pod.metadata.deletionTimestamp ||
    !pod.status.conditions?.some((condition: any) => condition.type === "Ready" && condition.status === "True"))) return false;
  for (const pod of pods) {
    const output = await kubectl(["exec", pod.metadata.name, "--", "printenv", "APP_MESSAGE"], undefined, true);
    if (output.exitCode || output.stdout.trim() !== value) return false;
  }
  return true;
}
async function workloadSnapshot() {
  const deployment = await get("deployment", "nginx");
  const pods = await nginxPods();
  return {
    uid: deployment?.metadata.uid,
    template: createHash("sha256").update(JSON.stringify(deployment?.spec.template)).digest("hex"),
    podUids: pods.map((pod: any) => pod.metadata.uid as string).sort(),
    activeReplicaSets: [...new Set(pods.map((pod: any) => pod.metadata.ownerReferences?.find((owner: any) => owner.kind === "ReplicaSet")?.uid as string))].sort(),
  };
}
function differentPods(before: Awaited<ReturnType<typeof workloadSnapshot>>, after: Awaited<ReturnType<typeof workloadSnapshot>>) {
  return after.podUids.length > 0 && after.podUids.every((uid: string) => !before.podUids.includes(uid));
}
const controllerArgs = ["run", "--kubernetes", "--namespace", namespace, "--state", "/state"];
const stateVolume = { name: "state", hostPath: { path: `/var/local/${cluster}/state`, type: "DirectoryOrCreate" } };
const container = {
  name: "controller", image, imagePullPolicy: "Never", args: controllerArgs,
  volumeMounts: [{ name: "state", mountPath: "/state" }],
};

async function main() {
  const clusters = (await command(["kind", "get", "clusters"])).stdout.split(/\s+/);
  if (clusters.includes(cluster)) throw new Error(`Cluster ${cluster} already exists. Set TSX_E2E_CLUSTER to a fresh name; this script will not overwrite it.`);
  const sources = sourceManifest();
  writeFileSync(join(directory, "source-sha256.json"), JSON.stringify(sources, null, 2));
  evidence.sourceManifest = "source-sha256.json";
  console.log(`Building standalone Linux controller ${image}`);
  await command(["docker", "build", "-t", image, "."]);
  const [built] = JSON.parse((await command(["docker", "image", "inspect", image])).stdout);
  evidence.imageId = built.Id;
  evidence.imagePlatform = `${built.Os}/${built.Architecture}`;
  evidence.imageCreated = built.Created;
  console.log(`Creating isolated cluster ${cluster}; your default kubeconfig is unchanged.`);
  await command(["kind", "create", "cluster", "--name", cluster, "--kubeconfig", kubeconfig, "--wait", "120s"]);
  await command(["kind", "load", "docker-image", image, "--name", cluster]);
  evidence.kubernetes = JSON.parse((await kubectl(["version", "-o", "json"])).stdout);
  const nodeList = JSON.parse((await kubectl(["get", "nodes", "-o", "json"])).stdout);
  evidence.nodes = nodeList.items.map((node: { metadata: { name: string }; status: { nodeInfo: unknown } }) => ({
    name: node.metadata.name, info: node.status.nodeInfo,
  }));

  const planned = await command([process.execPath, "run", "src/cli.ts", "plan", "--kubernetes", "--namespace", namespace]);
  const plan = JSON.parse(planned.stdout);
  const roles = plan.permissions?.roles ?? plan.roles;
  assert(Array.isArray(roles) && roles.length === 1, "Plan must contain exactly one generated namespace Role");
  writeFileSync(join(directory, "plan.json"), JSON.stringify(plan, null, 2));
  await apply([
    { apiVersion: "v1", kind: "Namespace", metadata: { name: namespace } },
    { apiVersion: "v1", kind: "ServiceAccount", metadata: { name: "tsx-controller", namespace } },
    ...roles,
    {
      apiVersion: "rbac.authorization.k8s.io/v1", kind: "RoleBinding",
      metadata: { name: "tsx-controller", namespace },
      subjects: [{ kind: "ServiceAccount", name: "tsx-controller", namespace }],
      roleRef: { apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: roles[0].metadata.name },
    },
    {
      apiVersion: "apps/v1", kind: "Deployment", metadata: { name: "tsx-controller", namespace },
      spec: { replicas: 1, strategy: { type: "Recreate" }, selector: { matchLabels: { app: "tsx-controller" } },
        template: { metadata: { labels: { app: "tsx-controller" } }, spec: {
          serviceAccountName: "tsx-controller", containers: [container], volumes: [stateVolume],
        } } },
    },
  ]);

  for (const [verb, resource, ns, expected] of [
    ["get", "deployments.apps/nginx", namespace, "yes"],
    ["get", "configmaps/nginx-config", namespace, "yes"],
    ["list", "configmaps/nginx-config", namespace, "yes"],
    ["watch", "configmaps/nginx-config", namespace, "yes"],
    ["patch", "configmaps/nginx-config", namespace, "no"],
    ["delete", "configmaps/nginx-config", namespace, "no"],
    ["create", "configmaps", namespace, "no"],
    ["delete", "configmaps/unrelated", namespace, "no"],
    ["get", "secrets", namespace, "no"],
    ["get", "deployments.apps/nginx", "default", "no"],
  ]) {
    const result = await kubectl(["auth", "can-i", verb!, resource!, "--namespace", ns!, "--as", `system:serviceaccount:${namespace}:tsx-controller`], undefined, true);
    assert.equal(result.stdout.trim(), expected, `RBAC ${verb} ${resource} in ${ns}`);
  }
  for (const rule of roles[0].rules) {
    if (rule.resources.includes("configmaps")) {
      assert.deepEqual([...rule.verbs].sort(), ["get", "list", "watch"]);
      assert.deepEqual(rule.resourceNames, ["nginx-config"]);
    }
  }
  evidence.rbac = "generated Role permits get/list/watch of nginx-config; denies ConfigMap create/patch/delete, Secrets, and other namespace";
  console.log("PASS generated Role and access boundaries");
  await kubectl(["rollout", "status", "deployment/tsx-controller", "--timeout=180s"]);
  await stableFor("missing required ConfigMap prevents initial workload creation", async () =>
    !await get("deployment", "nginx") && !await get("service", "nginx"));
  evidence.initialMissing = true;
  await apply([{ apiVersion: "v1", kind: "ConfigMap", metadata: { name: "nginx-config", namespace }, data: { APP_MESSAGE: "one" } }]);
  const impersonate = ["--as", `system:serviceaccount:${namespace}:tsx-controller`];
  for (const args of [
    ["patch", "configmap/nginx-config", "--type=merge", "-p", JSON.stringify({ data: { APP_MESSAGE: "forbidden" } })],
    ["delete", "configmap/nginx-config"],
    ["create", "configmap", "forbidden", "--from-literal=APP_MESSAGE=forbidden"],
  ]) {
    const denied = await kubectl([...args, ...impersonate], undefined, true);
    assert.notEqual(denied.exitCode, 0);
    assert.match(denied.stderr, /forbidden/i);
  }
  assert.equal((await get("configmap", "nginx-config")).data.APP_MESSAGE, "one");
  evidence.actualConfigMutationDenied = ["patch", "delete", "create"];
  await waitFor("nginx available with two replicas", async () => {
    const deployment = await get("deployment", "nginx");
    return deployment?.spec.replicas === 2 && deployment?.status.availableReplicas === 2;
  });
  const original = await get("deployment", "nginx");
  assert(original.metadata.finalizers.length > 0, "Managed Deployment has a registered finalizer");
  evidence.nginxUid = original.metadata.uid;
  await waitFor("Service created", async () => Boolean(await get("service", "nginx")));

  await waitFor("initial ConfigMap environment reaches every nginx Pod", () => podsHaveEnvironment("one", 2));
  const initial = await workloadSnapshot();
  evidence.initialConfigUid = (await get("configmap", "nginx-config")).metadata.uid;
  await kubectl(["patch", "configmap/nginx-config", "--type=merge", "-p", JSON.stringify({ data: { APP_MESSAGE: "two" } })]);
  await waitFor("ConfigMap content change rolls new Pods with updated environment", async () => {
    const current = await workloadSnapshot();
    return current.template !== initial.template && differentPods(initial, current) && await podsHaveEnvironment("two", 2);
  });
  const changed = await workloadSnapshot();
  assert.notDeepEqual(changed.activeReplicaSets, initial.activeReplicaSets, "ConfigMap content must create a new ReplicaSet");
  evidence.configRollout = { initial, changed };
  await kubectl(["annotate", "configmap/nginx-config", "e2e.example.com/metadata-only=true"]);
  await stableFor("metadata-only ConfigMap update leaves Pod template and Pods unchanged", async () =>
    JSON.stringify(await workloadSnapshot()) === JSON.stringify(changed));
  evidence.metadataOnly = changed;
  await kubectl(["delete", "configmap/nginx-config"]);
  await stableFor("missing ConfigMap preserves existing Deployment and Pods", async () =>
    JSON.stringify(await workloadSnapshot()) === JSON.stringify(changed));
  evidence.missingPreserves = changed;
  const watchLog = (await kubectl(["logs", "deployment/tsx-controller"])).stdout;
  writeFileSync(join(directory, "controller-before-restart.log"), watchLog);
  assert.match(watchLog, /resource-watch-change/, "Real Kubernetes watch delivered change events");
  evidence.watchEventObserved = true;
  await kubectl(["delete", "pod", "-l", "app=tsx-controller", "--wait=true"]);
  await kubectl(["rollout", "status", "deployment/tsx-controller", "--timeout=180s"]);
  await stableFor("controller restart while ConfigMap absent preserves durable desired workload", async () =>
    JSON.stringify(await workloadSnapshot()) === JSON.stringify(changed));
  evidence.restartWhileMissing = changed;
  const oldConfigUid = evidence.initialConfigUid;
  await apply([{ apiVersion: "v1", kind: "ConfigMap", metadata: { name: "nginx-config", namespace }, data: { APP_MESSAGE: "three" } }]);
  await waitFor("recreated unmanaged ConfigMap is observed and rolls updated environment", async () => {
    const current = await workloadSnapshot();
    return current.template !== changed.template && differentPods(changed, current) && await podsHaveEnvironment("three", 2);
  });
  const recreated = await workloadSnapshot();
  assert.notEqual((await get("configmap", "nginx-config")).metadata.uid, oldConfigUid);
  evidence.configRecreated = recreated;

  const probe = await kubectl(["run", "http-probe", "--image=busybox:1.37", "--restart=Never", "--attach=true", "--rm",
    "--pod-running-timeout=120s", "--command", "--", "wget", "-q", "-O", "-", `http://nginx.${namespace}.svc.cluster.local`]);
  assert.match(probe.stdout, /Welcome to nginx!/);
  evidence.clusterHttp = "nginx welcome page via Service DNS and ClusterIP from a separate Pod";
  console.log("PASS in-cluster HTTP through Service DNS and ClusterIP");

  // Let kubectl choose a free local port, then exercise the real Service endpoint.
  const forward = Bun.spawn(["kubectl", "--kubeconfig", kubeconfig, "--context", context, "-n", namespace,
    "port-forward", "service/nginx", ":80", "--address", "127.0.0.1"], { stdout: "pipe", stderr: "pipe" });
  try {
    const reader = forward.stdout.getReader();
    let output = "";
    let port: string | undefined;
    const abort = setTimeout(() => forward.kill(), 30_000);
    try {
      while (!port) {
        const chunk = await reader.read();
        if (chunk.done) throw new Error(`Port-forward ended: ${output}`);
        output += new TextDecoder().decode(chunk.value);
        port = output.match(/127\.0\.0\.1:(\d+) -> 80/)?.[1];
      }
      const response = await fetch(`http://127.0.0.1:${port}`, { signal: AbortSignal.timeout(10_000) });
      const body = await response.text();
      assert.equal(response.status, 200);
      assert.match(body, /Welcome to nginx!/);
      evidence.http = { status: response.status, contains: "Welcome to nginx!" };
      console.log("PASS HTTP 200 through Kubernetes Service");
    } finally { clearTimeout(abort); reader.releaseLock(); }
  } finally { forward.kill(); await forward.exited; }

  await kubectl(["set", "env", "deployment/tsx-controller", "NGINX_REPLICAS=3"]);
  await kubectl(["rollout", "status", "deployment/tsx-controller", "--timeout=180s"]);
  await waitFor("desired input update rolls nginx to three replicas", async () => {
    const deployment = await get("deployment", "nginx");
    return deployment?.spec.replicas === 3 && deployment?.status.availableReplicas === 3;
  });
  evidence.desiredUpdate = "NGINX_REPLICAS=3 updates existing nginx Deployment";
  await kubectl(["scale", "deployment/nginx", "--replicas", "1"]);
  await waitFor("replica drift repaired", async () => (await get("deployment", "nginx"))?.spec.replicas === 3);
  assert.equal((await get("deployment", "nginx")).metadata.uid, original.metadata.uid);
  evidence.driftRepaired = true;
  await kubectl(["delete", "pod", "-l", "app=tsx-controller", "--wait=true"]);
  await kubectl(["rollout", "status", "deployment/tsx-controller", "--timeout=180s"]);
  // A second drift proves the replacement process acquired the persistent state and reconciles.
  await kubectl(["scale", "deployment/nginx", "--replicas", "1"]);
  await waitFor("replacement controller repairs drift with same nginx UID", async () => {
    const deployment = await get("deployment", "nginx");
    return deployment?.spec.replicas === 3 && deployment?.metadata.uid === original.metadata.uid;
  });
  evidence.restart = "new controller Pod, persistent state, same nginx UID, drift repaired";
  await waitFor("all three replacement Pods retain latest ConfigMap environment", () => podsHaveEnvironment("three", 3));

  await kubectl(["logs", "deployment/tsx-controller"]).then(result => writeFileSync(join(directory, "controller.log"), result.stdout));
  await kubectl(["scale", "deployment/tsx-controller", "--replicas", "0"]);
  await kubectl(["wait", "--for=delete", "pod", "-l", "app=tsx-controller", "--timeout=120s"]);
  await kubectl(["delete", "deployment/nginx", "--wait=false"]);
  const pending = await get("deployment", "nginx");
  assert(pending?.metadata.deletionTimestamp, "Deletion must be held by registered finalizer");
  const configBeforeFinalize = await get("configmap", "nginx-config");
  assert(configBeforeFinalize, "Unmanaged ConfigMap remains present");
  console.log("PASS deletion held while controller is stopped");
  await apply([{
    apiVersion: "batch/v1", kind: "Job", metadata: { name: "tsx-finalize", namespace },
    spec: { backoffLimit: 0, activeDeadlineSeconds: 180, template: { spec: {
      serviceAccountName: "tsx-controller", restartPolicy: "Never", volumes: [stateVolume],
      containers: [{ ...container, args: ["finalize", "--kubernetes", "--namespace", namespace, "--state", "/state"] }],
    } } },
  }]);
  await kubectl(["wait", "--for=condition=complete", "job/tsx-finalize", "--timeout=180s"]);
  await waitFor("inline finalizer releases managed Deployment and Service deletion", async () =>
    !await get("deployment", "nginx") && !await get("service", "nginx"));
  const finalizerLog = (await kubectl(["logs", "job/tsx-finalize"])).stdout;
  writeFileSync(join(directory, "finalizer.log"), finalizerLog);
  assert.match(finalizerLog, /managed-finalizer-complete/);
  const configAfterFinalize = await get("configmap", "nginx-config");
  assert.equal(configAfterFinalize?.metadata.uid, configBeforeFinalize.metadata.uid);
  assert.deepEqual(configAfterFinalize.data, configBeforeFinalize.data);
  evidence.finalizer = { managedResourcesDeleted: true, hookExecuted: true,
    unmanagedConfigUid: configAfterFinalize.metadata.uid, unmanagedConfigData: configAfterFinalize.data };
  assert.deepEqual(sourceManifest(), sources, "Source files changed during acceptance; rerun with the final source");
  evidence.sourceUnchanged = true;
  evidence.status = "passed";
  console.log(`All local Kubernetes checks passed. Cluster preserved: ${cluster}\nEvidence: ${directory}\nInspect: kubectl --kubeconfig ${kubeconfig} --context ${context} -n ${namespace} get all`);
}

try { await main(); }
catch (error) {
  evidence.status = "failed";
  evidence.error = String(error);
  const diagnostics = await kubectl(["get", "pods", "-o", "wide"], undefined, true).catch(() => undefined);
  if (diagnostics) console.error(diagnostics.stdout, diagnostics.stderr);
  const logs = await kubectl(["logs", "-l", "app=tsx-controller", "--tail=100"], undefined, true).catch(() => undefined);
  if (logs) writeFileSync(join(directory, "failure-controller.log"), logs.stdout + logs.stderr);
  console.error(error);
  process.exitCode = 1;
} finally { writeFileSync(join(directory, "result.json"), JSON.stringify(evidence, null, 2)); }
