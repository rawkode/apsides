import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { website } from "../examples/website.tsx";
import { ControllerRunner } from "./controller.ts";
import { createKubernetesEngine } from "./kubernetes/index.ts";
import { permissions } from "./permissions.ts";
import { prepareReactive } from "./reactive";
import { readInventory } from "./state.ts";

const { values, positionals } = parseArgs({
  args: process.argv.slice(2), allowPositionals: true,
  options: {
    state: { type: "string", default: "/state" }, interval: { type: "string", default: "2000" },
    namespace: { type: "string", default: process.env.POD_NAMESPACE ?? "tsx-e2e" },
    timeout: { type: "string", default: "120000" },
    kubernetes: { type: "boolean" }, help: { type: "boolean" },
  },
});
const command = positionals[0] ?? "help";
const print = (value: unknown) => console.log(JSON.stringify(value));
if (values.help || command === "help") {
  console.log(`TSX controller — real Kubernetes through Alchemy

controller plan --namespace NAME         Print generated workload Role requirements
controller reconcile --state DIRECTORY   Reconcile once (blocked: exit 2)
controller run --state DIRECTORY         Reconcile continuously (--interval milliseconds)
controller status --state DIRECTORY      Observe managed Kubernetes resources
controller finalize --state DIRECTORY [RESOURCE_ID ...]

Run inside Kubernetes using its mounted service-account token and CA.
All runtime commands use real Kubernetes; --kubernetes is accepted explicitly.
The executable embeds examples/website.tsx. NGINX_REPLICAS and NGINX_IMAGE
configure the local example. Run 'bun run e2e' on the host for local kind acceptance.`);
} else {
  try { await main(); }
  catch (error) { console.error(error instanceof Error ? error.message : "Controller failed"); process.exitCode = 1; }
}

async function main() {
  const graph = website({ namespace: values.namespace });
  if (command === "plan") {
    print({ controller: graph.name, resources: graph.resources.map(({ props, finalizers, ...resource }) => ({
      ...resource, finalizers: Object.keys(finalizers),
    })), permissions: permissions(graph) });
    return;
  }
  if (!["reconcile", "run", "status", "finalize"].includes(command)) throw new Error(`Unknown command: ${command}`);
  const directory = resolve(values.state!);
  const engine = createKubernetesEngine({ directory, namespace: graph.namespace, graph });
  const prepare = prepareReactive(directory, graph,
    snapshots => website({ namespace: graph.namespace, snapshots }), target => engine.readObserved(target));
  const runner = new ControllerRunner(graph, engine, { directory, prepare });
  if (command === "status") {
    print(await Promise.all(graph.resources.map(resource => engine.observe(resource.id)))); return;
  }
  if (command === "finalize") {
    const timeout = Number(values.timeout);
    if (!Number.isFinite(timeout) || timeout < 1) throw new Error("Timeout must be positive milliseconds");
    const deadline = Date.now() + timeout;
    let selected = positionals.length > 1 ? positionals.slice(1) : undefined;
    for (;;) {
      const report = await runner.finalizeOnly(selected);
      print(report);
      if (report.phase === "ready") return;
      const retryable = report.conditions.every(({ reason }) =>
        reason === "DeletionPending" || reason === "DependentCleanupBlocked" || reason.startsWith("FinalizerFailed:"));
      if (!retryable || Date.now() >= deadline) { process.exitCode = 2; return; }
      await Bun.sleep(1000);
      if (selected) {
        const inventory = readInventory(directory, graph.name);
        selected = selected.filter(id => id in inventory.resources);
      }
    }
  }
  if (command === "reconcile") {
    const report = await runner.reconcile(); print(report);
    if (report.phase === "blocked") process.exitCode = 2;
    return;
  }
  const interval = Number(values.interval);
  if (!Number.isFinite(interval) || interval < 50) throw new Error("Interval must be at least 50 milliseconds");
  const stop = new AbortController();
  process.once("SIGINT", () => stop.abort()); process.once("SIGTERM", () => stop.abort());
  while (!stop.signal.aborted) {
    try { print(await runner.reconcile()); }
    catch (error) { print({ phase: "error", reason: error instanceof Error ? error.message : "Reconcile failed" }); }
    const reason = await engine.waitForObservationChange(graph.observations, stop.signal, interval);
    if (reason === "watch") print({ event: "resource-watch-change" });
  }
}
