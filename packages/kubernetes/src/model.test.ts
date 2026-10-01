import {
  ConfigMap,
  Deployment,
  NamespacedResource,
  Secret,
  Service,
  defineDomain,
  defineHandler,
  defineHandlerOutput,
  fixtureInputData,
  watch,
  type ContainerDefinition,
  type DomainBudgets,
  type SourceLocation,
} from "./model.ts";

const assert: (condition: unknown, message: string) => asserts condition = (condition, message) => {
  if (!condition) throw new Error(message);
};

const equal = (actual: unknown, expected: unknown, message: string): void => {
  if (actual !== expected) throw new Error(`${message}: expected ${String(expected)}, received ${String(actual)}`);
};

const throws = (run: () => unknown, expected: string): void => {
  try {
    run();
  } catch (error) {
    assert(error instanceof Error && error.message.includes(expected), `expected error containing ${expected}, received ${String(error)}`);
    return;
  }
  throw new Error(`expected error containing ${expected}`);
};

const containsFunction = (value: unknown, seen = new Set<object>()): boolean => {
  if (typeof value === "function") return true;
  if (value === null || typeof value !== "object" || seen.has(value)) return false;
  seen.add(value);
  return Object.values(value).some((child) => containsFunction(child, seen));
};

const source: SourceLocation = { file: "domain.ts", line: 10, column: 3 };
const budgets: DomainBudgets = {
  maxResources: 8,
  maxInputs: 8,
  maxReactions: 8,
  maxObjectBytes: 32_768,
  maxQueueDepth: 32,
  maxConcurrentRequests: 2,
};
const sha256 = "0123456789abcdef".repeat(4);

const domain = (id = "test") => {
  return defineDomain({
    id,
    namespace: "workload",
    controlNamespace: "apsides-system",
    clusterBinding: "local-fixture",
    policyRef: "platform/test",
    enforcement: "api-bound",
    budgets,
    provenance: { inputs: [{ path: "domain.ts", sha256 }] },
  });
};

const deployment = (id = "api", containers = [{ name: "api", image: "example.test/api@sha256:abc" }]) => {
  return new Deployment({
    id,
    metadata: { name: id, namespace: "workload" },
    spec: {
      replicas: 2,
      selector: { matchLabels: { app: id } },
      template: {
        metadata: { labels: { app: id } },
        spec: { serviceAccountName: "workload", containers },
      },
    },
  });
};

const deploymentWithEnv = (env: NonNullable<ContainerDefinition["env"]>) => {
  return new Deployment({
    metadata: { name: "postgrest", namespace: "workload" },
    spec: {
      selector: { matchLabels: { app: "postgrest" } },
      template: {
        metadata: { labels: { app: "postgrest" } },
        spec: { containers: [{ name: "postgrest", image: "example.test/postgrest@sha256:abc", env }] },
      },
    },
  });
};

const build = (order: "service-first" | "service-last") => {
  const value = domain();
  const service = () => value.manage(new Service({
    id: "service",
    metadata: { name: "api", namespace: "workload" },
    spec: { selector: { app: "api" }, ports: [{ name: "http", port: 80, targetPort: 8080 }] },
  }), { source });
  if (order === "service-first") service();
  const config = value.manage(new ConfigMap({ id: "config", metadata: { name: "api-config", namespace: "workload" }, data: { Z: "last", A: "first" } }), { deletionPolicy: "orphan", source });
  const target = value.manage(deployment(), { source });
  if (order === "service-last") service();
  const credentials = value.observe(Secret, { name: "credentials", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  watch(config).project("data").rollout(target, { id: "rollout-config", source });
  watch(credentials).project("data").rollout(target, { id: "rollout-secret", source });
  return value;
};

const first = build("service-first");
const second = build("service-last");
const ir = first.toIR();
equal(JSON.stringify(ir), JSON.stringify(second.toIR()), "declaration order must not affect IR");
equal(ir.schemaVersion, "apsides.dev/ir/v2", "schema version");
equal(ir.handlers.length, 0, "no handlers declared");
equal(ir.outputs.length, 0, "no outputs declared");
equal(ir.testOnly, false, "author IR is production-marked");
equal(ir.resources.map((resource) => resource.id).join(","), "api,config,service", "resources sorted by stable ID");
equal(ir.inputs.length, 2, "managed and observed inputs emitted");
equal(ir.inputs.find((input) => input.kind === "Secret")?.sensitive, true, "Secret sensitivity retained");
equal(ir.reactions.length, 2, "rollout reactions emitted");
equal(ir.provenance.compiler, "apsides/0.1", "compiler provenance fixed by SDK");
equal(ir.domain.minKubernetesVersion, "1.34", "minKubernetesVersion defaults to the qualified floor");
assert(!("requestPlan" in ir) && !("testPlan" in ir) && !("capabilities" in ir), "derived compiler output must not be author-supplied");
assert(!containsFunction(ir), "IR must contain no runtime callbacks");
assert(Object.isFrozen(ir) && Object.isFrozen(ir.resources), "IR snapshots must be immutable");

const clusterDescriptor = {
  apiVersion: "postgresql.cnpg.io/v1",
  kind: "Cluster",
  apiResource: {
    plural: "clusters",
    scope: "Namespaced" as const,
    schemaSha256: `sha256:${sha256}`,
  },
};
const custom = domain("custom");
custom.manage(new NamespacedResource(clusterDescriptor, {
  id: "database",
  metadata: { name: "database", namespace: "workload" },
  spec: { instances: 2, storage: { size: "1Gi" } },
}), { deletionPolicy: "delete", source });
const customDeclaration = custom.toIR().resources[0]!;
equal(customDeclaration.apiVersion, "postgresql.cnpg.io/v1", "custom apiVersion is descriptor-owned");
equal(customDeclaration.kind, "Cluster", "custom kind is descriptor-owned");
equal(customDeclaration.apiResource?.plural, "clusters", "custom discovery metadata is carried into IR");
equal(customDeclaration.apiResource?.schemaSha256, `sha256:${sha256}`, "custom schema digest is carried into IR");
equal((customDeclaration.desired as unknown as { spec: { instances: number } }).spec.instances, 2, "custom spec is preserved as JSON");
assert(ir.resources.every((resource) => resource.apiResource === undefined), "built-in resources omit custom discovery metadata");
throws(() => new NamespacedResource(clusterDescriptor, {
  metadata: { name: "bad", namespace: "workload" },
  spec: new Date(),
}), "plain data object");
throws(() => new NamespacedResource({
  ...clusterDescriptor,
  apiResource: { ...clusterDescriptor.apiResource, schemaSha256: `sha256:${sha256.toUpperCase()}` },
}, { metadata: { name: "bad", namespace: "workload" }, spec: {} }), "lowercase sha256 digest");
throws(() => new NamespacedResource({
  ...clusterDescriptor,
  kind: "Deployment",
}, { metadata: { name: "bad", namespace: "workload" }, spec: {} }), "cannot replace built-in");
throws(() => custom.manage(new NamespacedResource({
  ...clusterDescriptor,
  apiVersion: "postgresql.cnpg.io/v1beta1",
  kind: "DatabaseCluster",
}, {
  metadata: { name: "database", namespace: "workload" },
  spec: {},
}), { source }), "already declared");

const secretConsumers = domain("secret-consumers");
secretConsumers.observe(Secret, { name: "database-app", namespace: "workload", source, projection: ["data"], missing: "block-dependent-mutations" });
secretConsumers.manage(new Deployment({
  metadata: { name: "database-api", namespace: "workload" },
  spec: { selector: { matchLabels: { app: "database-api" } }, template: {
    metadata: { labels: { app: "database-api" } }, spec: { containers: [
      { name: "api", image: "example.test/api", env: [
        { name: "URI", valueFrom: { secretKeyRef: { name: "database-app", key: "uri" } } },
        { name: "OTHER", valueFrom: { secretKeyRef: { name: "other-secret", key: "unrelated" } } },
      ] },
      { name: "sidecar", image: "example.test/sidecar", env: [
        { name: "USER", valueFrom: { secretKeyRef: { name: "database-app", key: "username" } } },
        { name: "REPEATED", valueFrom: { secretKeyRef: { name: "database-app", key: "uri" } } },
      ] },
    ] },
  } },
}), { source });
const secretIr = secretConsumers.toIR();
const secretSeed = fixtureInputData(secretIr.inputs[0]!, secretIr.resources);
equal(Object.keys(secretSeed).sort().join(","), "uri,username,value", "all required Secret keys are seeded once without unrelated refs");
equal(secretSeed.uri, secretSeed.value, "required keys contain deterministic synthetic data");
const otherNamespace = fixtureInputData(secretIr.inputs[0]!, secretIr.resources.map((resource) => ({ ...resource, namespace: "other" })));
equal(Object.keys(otherNamespace).join(","), "value", "Secret refs in another namespace do not add keys");

const originalData: Record<string, string> = { key: "before" };
const snapDomain = domain("snapshot");
snapDomain.manage(new ConfigMap({ metadata: { name: "snap", namespace: "workload" }, data: originalData }), { source });
originalData.key = "after";
equal((snapDomain.toIR().resources[0]?.desired as { data?: { key?: string } }).data?.key, "before", "constructor snapshots author data");

throws(() => new ConfigMap({ metadata: { name: "bad", namespace: "workload" }, data: new Date() as never }), "plain data");
throws(() => new ConfigMap({ metadata: { name: "bad", namespace: "workload" }, data: { bad: undefined } as never }), "must be a string");
const accessor = { metadata: { name: "bad", namespace: "workload" } } as { metadata: object; data?: object };
Object.defineProperty(accessor, "data", { enumerable: true, get: () => ({ secret: "ambient" }) });
throws(() => new ConfigMap(accessor as never), "data property");
throws(() => new Deployment({
  metadata: { name: "bad", namespace: "workload" },
  spec: { selector: { matchLabels: { app: "bad" } }, template: { metadata: { labels: { app: "bad" }, annotations: { "apsides.dev/revision": "forged" } }, spec: { containers: [{ name: "bad", image: "bad" }] } } },
}), "reserved");

const tokenless = domain("tokenless");
tokenless.manage(new Deployment({
  metadata: { name: "api", namespace: "workload" },
  spec: {
    selector: { matchLabels: { app: "api" } },
    template: {
      metadata: { labels: { app: "api" } },
      spec: {
        automountServiceAccountToken: false,
        containers: [{ name: "api", image: "example.test/api@sha256:abc" }],
      },
    },
  },
}), { source });
const tokenlessDesired = tokenless.toIR().resources[0]?.desired as unknown as {
  spec: { template: { spec: { automountServiceAccountToken?: boolean } } };
};
equal(tokenlessDesired.spec.template.spec.automountServiceAccountToken, false, "disabled ServiceAccount token automount is preserved");
throws(() => new Deployment({
  metadata: { name: "bad", namespace: "workload" },
  spec: {
    selector: { matchLabels: { app: "bad" } },
    template: {
      metadata: { labels: { app: "bad" } },
      spec: {
        automountServiceAccountToken: "false",
        containers: [{ name: "bad", image: "bad" }],
      },
    },
  },
} as never), "must be a boolean");

const environment = domain("environment");
environment.manage(deploymentWithEnv([
  { name: "PGRST_DB_SCHEMAS", value: "public" },
  { name: "EMPTY_ALLOWED", value: "" },
  { name: "PGRST_DB_URI", valueFrom: { secretKeyRef: { name: "database-app", key: "uri" } } },
]), { source });
const normalizedEnv = (environment.toIR().resources[0]?.desired as unknown as {
  spec: { template: { spec: { containers: readonly ContainerDefinition[] } } };
}).spec.template.spec.containers[0]?.env;
equal(normalizedEnv?.[0]?.name, "PGRST_DB_SCHEMAS", "literal environment name is preserved");
equal("value" in normalizedEnv![1]! ? normalizedEnv![1]!.value : undefined, "", "empty literal environment values are supported");
equal("valueFrom" in normalizedEnv![2]! ? normalizedEnv![2]!.valueFrom.secretKeyRef.key : undefined, "uri", "Secret key reference is normalized");
assert(Object.isFrozen(normalizedEnv) && Object.isFrozen(normalizedEnv?.[2]), "environment snapshot is immutable");
throws(() => deploymentWithEnv([
  { name: "DUPLICATE", value: "one" },
  { name: "DUPLICATE", value: "two" },
]), "duplicate Deployment environment variable name");
throws(() => deploymentWithEnv([{
  name: "AMBIGUOUS",
  value: "plain",
  valueFrom: { secretKeyRef: { name: "database-app", key: "uri" } },
} as never]), "exactly one of value or valueFrom");
throws(() => deploymentWithEnv([{ name: "MISSING" } as never]), "exactly one of value or valueFrom");
throws(() => deploymentWithEnv([{
  name: "PGRST_DB_URI",
  valueFrom: { secretKeyRef: { name: "database-app", key: "" } },
}]), "must be a nonempty string");

const crossA = domain("a");
const crossB = domain("b");
const external = crossA.observe(ConfigMap, { name: "external", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
const otherTarget = crossB.manage(deployment("other"), { source });
throws(() => watch(external).project("data").rollout(otherTarget), "cross-domain");
throws(() => crossA.manage(new ConfigMap({ metadata: { name: "external", namespace: "workload" } }), { source }), "aliases observed");
throws(() => domain().observe(Secret, { name: "elsewhere", namespace: "other", projection: ["data"], missing: "block-dependent-mutations", source }), "outside domain namespace");

const missingProvenance = defineDomain({
  id: "missing-source",
  namespace: "workload",
  controlNamespace: "apsides-system",
  clusterBinding: "local-fixture",
  policyRef: "platform/test",
  enforcement: "api-bound",
  budgets,
  provenance: { inputs: [{ path: "other.ts", sha256 }] },
});
missingProvenance.manage(new ConfigMap({ metadata: { name: "config", namespace: "workload" } }), { source });
throws(() => missingProvenance.toIR(), "absent from provenance");

const constrained = defineDomain({
  id: "budget",
  namespace: "workload",
  controlNamespace: "apsides-system",
  clusterBinding: "local-fixture",
  policyRef: "platform/test",
  enforcement: "api-bound",
  budgets: { ...budgets, maxResources: 1 },
  provenance: { inputs: [{ path: "domain.ts", sha256 }] },
});
constrained.manage(new ConfigMap({ metadata: { name: "one", namespace: "workload" } }), { source });
throws(() => constrained.manage(new ConfigMap({ metadata: { name: "two", namespace: "workload" } }), { source }), "maxResources");

{
  const value = defineDomain({
    id: "defaults", namespace: "workload", controlNamespace: "apsides-system",
    clusterBinding: "local-fixture", policyRef: "platform/test", enforcement: "api-bound",
    budgets, force: true, prune: true, provenance: { inputs: [{ path: "domain.ts", sha256 }] },
  });
  value.manage(new ConfigMap({ metadata: { name: "inherited", namespace: "workload" } }), { source });
  value.manage(new ConfigMap({ metadata: { name: "overridden", namespace: "workload" } }), {
    source, force: false, deletionPolicy: "orphan",
  });
  const output = value.toIR();
  equal(output.resources[0]!.force, true, "controller force default resolves on resource");
  equal(output.resources[0]!.lifecycle.deletionPolicy, "delete", "prune default resolves to recorded deletion policy");
  equal(output.resources[1]!.force, undefined, "resource override disables force");
  equal(output.resources[1]!.lifecycle.deletionPolicy, "orphan", "explicit lifecycle overrides prune");
  equal("force" in output.domain, false, "force default is not ambient runtime configuration");
  equal("prune" in output.domain, false, "prune default is not retroactive cleanup authority");
}

{
  const labelPattern = /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/;
  const value = domain("derived-ids");
  const longName = `${"a".repeat(60)}.example.test`;
  const shared = "x".repeat(70);
  const target = value.manage(deployment("api"), { source });
  const first = value.observe(ConfigMap, { name: `${shared}-first`, namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  const second = value.observe(ConfigMap, { name: `${shared}-second`, namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  const dotted = value.manage(new ConfigMap({ metadata: { name: longName, namespace: "workload" } }), { source });
  const short = value.observe(Secret, { name: "short", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  watch(first).project("data").rollout(target, { source });
  watch(dotted).project("data").rollout(target, { source });
  watch(short).project("data").rollout(target, { source });
  const output = value.toIR();
  const ids = [...output.resources, ...output.inputs, ...output.reactions].map((declaration) => declaration.id);
  assert(ids.every((id) => id.length <= 63 && labelPattern.test(id)), `derived ids must be bounded DNS labels: ${ids.join(",")}`);
  assert(first.id !== second.id, "long derivations sharing a prefix remain distinct");
  equal(second.id, "observed-configmap-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-5474b7ed12", "bounded ids keep the readable derived prefix");
  equal(first.id, "observed-configmap-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-2d719c957c", "bounded observed id carries a hash of the full derivation");
  equal(dotted.id, "configmap-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-b045bae0c1", "dotted derived resource id is bounded");
  equal(short.id, "observed-secret-short", "short derived id is unchanged");
  equal(output.reactions.find((reaction) => reaction.inputs[0] === short.id)?.id, "rollout-observed-secret-short-api", "short derived rollout id is unchanged");
  equal(output.inputs.find((input) => input.mode === "managed")?.id, "managed-configmap-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-b9487f1d3e", "derived managed input id is bounded");
  throws(() => value.observe(ConfigMap, { id: "x".repeat(64), name: "explicit", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source }), "observe.id exceeds 63 characters");
  throws(() => value.manage(new ConfigMap({ id: "c".repeat(64), metadata: { name: "explicit", namespace: "workload" } }), { source }), "manage.id exceeds 63 characters");
  throws(() => watch(short).project("data").rollout(target, { id: `rollout-${"r".repeat(60)}`, source }), "rollout.id exceeds 63 characters");
  throws(() => watch(short).project("data").rollout(target, { id: "Rollout.Bad", source }), "rollout.id must be a lowercase DNS label");
  throws(() => watch(short).project("data").rollout(target, { source }), "duplicate reaction id: rollout-observed-secret-short-api");
}

{
  const options = {
    id: "min-version", namespace: "workload", controlNamespace: "apsides-system",
    clusterBinding: "local-fixture", policyRef: "platform/test", enforcement: "api-bound" as const,
    budgets, provenance: { inputs: [{ path: "domain.ts", sha256 }] },
  };
  throws(() => defineDomain({ ...options, minKubernetesVersion: "1.33" }), "at least the qualified floor 1.34");
  throws(() => defineDomain({ ...options, minKubernetesVersion: "1.34.3" }), "must be formatted as canonical MAJOR.MINOR");
  throws(() => defineDomain({ ...options, minKubernetesVersion: "2.0" }), "at least the qualified floor 1.34");
  throws(() => defineDomain({ ...options, minKubernetesVersion: "1.034" }), "must be formatted as canonical MAJOR.MINOR");
  throws(() => defineDomain({ ...options, minKubernetesVersion: "01.34" }), "must be formatted as canonical MAJOR.MINOR");
  throws(() => defineDomain({ ...options, minKubernetesVersion: "1.1234567890" }), "must be formatted as canonical MAJOR.MINOR");
  const raised = defineDomain({ ...options, minKubernetesVersion: "1.35" });
  raised.manage(new ConfigMap({ metadata: { name: "config", namespace: "workload" } }), { source });
  equal(raised.toIR().domain.minKubernetesVersion, "1.35", "explicit minKubernetesVersion is retained");
}

{
  // Handler outputs: an Output-mode input usable in rolloutOn, plus a
  // declared handler triggering on a sensitive-free ConfigMap input.
  const value = domain("handlers");
  const target = value.manage(deployment("api"), { source });
  const settings = value.observe(ConfigMap, { name: "settings", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  const flags = defineHandlerOutput(value, { name: "flags", source });
  watch(flags).project("data").rollout(target, { id: "rollout-flags", source });
  defineHandler(value, {
    id: "sync-flags",
    module: "./handlers/sync-flags.ts",
    export: "syncFlags",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: ["api.example.com", "other.example.com:8443"],
    outputs: [flags.outputId],
    source,
  });
  const output = value.toIR();
  equal(output.outputs.length, 1, "one output declared");
  equal(output.outputs[0]!.name, "flags", "output name preserved");
  const outputResource = output.resources.find((resource) => resource.id === flags.outputId);
  assert(outputResource !== undefined, "output resource declared");
  equal(outputResource!.handlerOutput, true, "output resource is flagged handlerOutput");
  equal((outputResource!.desired as unknown as { data?: unknown }).data, undefined, "output resource desired carries no data");
  equal(
    (outputResource!.desired as unknown as { metadata: { labels: Record<string, string> } }).metadata.labels["apsides.dev/handler-output"],
    flags.outputId,
    "reserved handler-output label carries the output id",
  );
  equal(output.handlers.length, 1, "one handler declared");
  equal(output.handlers[0]!.module, "handlers/sync-flags.ts", "handler module is normalized (leading ./ stripped)");
  equal(output.handlers[0]!.export, "syncFlags", "handler export preserved");
  equal(output.handlers[0]!.egress.length, 2, "handler egress hosts preserved");
  equal(output.handlers[0]!.egress[1]!.port, 8443, "explicit egress port preserved");
  equal(output.handlers[0]!.egress[0]!.port, 443, "egress port defaults to 443");
  equal(output.handlers[0]!.bundleSha256, "", "SDK emits an empty bundleSha256 placeholder for the compiler to fill");
  assert(
    output.inputs.some((input) => input.mode === "output" && input.id === flags.outputId && input.managedResourceId === flags.outputId),
    "an Output-mode input with the output's own id backs the handler output",
  );

  // That id is the output's, so an author input cannot reuse it.
  const clash = domain("handler-id-clash");
  clash.observe(ConfigMap, { id: "flags", name: "external-flags", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  throws(() => defineHandlerOutput(clash, { id: "flags", name: "flags", source }), "duplicate input id: flags");

  // An output declared first under a managed input's derived id cannot stand in
  // for that managed ConfigMap's watch input.
  const takeover = domain("handler-managed-takeover");
  defineHandlerOutput(takeover, { id: "managed-settings", name: "derived", source });
  const managedSettings = takeover.manage(new ConfigMap({ id: "settings", metadata: { name: "settings", namespace: "workload" }, data: { a: "1" } }), { source });
  const api = takeover.manage(deployment("api"), { source });
  throws(() => watch(managedSettings).project("data").rollout(api, { id: "rollout-settings", source }), "duplicate input id: managed-settings");

  throws(() => defineHandler(value, {
    id: "sync-flags",
    module: "./handlers/other.ts",
    export: "other",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: [],
    outputs: [],
    source,
  }), "duplicate handler id");

  // Each output has exactly one writer handler.
  throws(() => defineHandler(value, {
    module: "./handlers/other.ts",
    export: "other",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: [],
    outputs: [flags.outputId],
    source,
  }), "already has a writer handler");
}

{
  // 16 distinct hosts (at, not over, MAX_EGRESS_HOSTS) times 17 distinct
  // ports each: 272 rows, over MAX_EGRESS_ROWS (256) despite never
  // tripping the distinct-host budget.
  const value = domain("egress-rows");
  const settings = value.observe(ConfigMap, { name: "settings", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  const egress: string[] = [];
  for (let hostIndex = 0; hostIndex < 16; hostIndex++) {
    for (let portOffset = 0; portOffset < 17; portOffset++) {
      egress.push(`host-${String(hostIndex)}.example.com:${String(443 + portOffset)}`);
    }
  }
  equal(egress.length, 272, "272 egress rows constructed");
  throws(() => defineHandler(value, {
    module: "./handlers/bad.ts",
    export: "bad",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress,
    outputs: [],
    source,
  }), "maxEgressRows");
}

{
  // maxQueueDepth must hold every handler: each handler keeps at most one
  // queue slot, and a smaller queue could fill with downstream handlers
  // held for an upstream that then can never enqueue.
  const value = defineDomain({
    id: "queue-depth", namespace: "workload", controlNamespace: "apsides-system", clusterBinding: "local-fixture",
    policyRef: "platform/test", enforcement: "api-bound", budgets: { ...budgets, maxQueueDepth: 1 },
    provenance: { inputs: [{ path: "domain.ts", sha256 }] },
  });
  const settings = value.observe(ConfigMap, { name: "settings", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  const handler = (id: string) => ({
    id, module: "./handlers/notify.ts", export: "notify",
    trigger: { kind: "input-changed" as const, input: settings.id }, inputs: [settings.id], egress: [], outputs: [], source,
  });
  defineHandler(value, handler("notify-a"));
  throws(() => defineHandler(value, handler("notify-b")), "cannot hold every handler");
}

{
  // A derived handler id lowercases a camelCase export instead of turning
  // each capital into a separator, so `syncFlags` stays readable.
  const value = domain("derived-handler-id");
  const settings = value.observe(ConfigMap, { name: "settings", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  defineHandler(value, {
    module: "./handlers/sync-flags.ts",
    export: "syncFlags",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: [],
    outputs: [],
    source,
  });
  const id = value.toIR().handlers[0]!.id;
  assert(/^handler-syncflags---handlers-sync-flags-ts-[0-9a-f]{10}$/.test(id), `derived handler id is readable: ${id}`);
}

{
  // A closure is never a valid handler reference at the runtime.ts JSX
  // layer; model.ts itself only ever accepts an already-split module/export
  // pair, so the "closures are rejected" contract is exercised in
  // traversal.test.ts against the `<On>` element.
  const value = domain("handler-rules");
  const secret = value.observe(Secret, { name: "credentials", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  throws(() => defineHandler(value, {
    module: "./handlers/leak.ts",
    export: "leak",
    trigger: { kind: "input-changed", input: secret.id },
    inputs: [secret.id],
    egress: [],
    outputs: [],
    source,
  }), "non-sensitive ConfigMap input");

  const settings = value.observe(ConfigMap, { name: "settings", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  throws(() => defineHandler(value, {
    module: "../outside/handler.ts",
    export: "bad",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: [],
    outputs: [],
    source,
  }), "normalized relative path");
  throws(() => defineHandler(value, {
    module: "./handlers/bad.ts",
    export: "bad",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: ["10.0.0.1"],
    outputs: [],
    source,
  }), "IP literal");
  throws(() => defineHandler(value, {
    module: "./handlers/bad.ts",
    export: "bad",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: ["*.example.com"],
    outputs: [],
    source,
  }), "wildcard");
  for (const host of ["127.1", "2130706433", "0x7f.1", "0177.0.0.1", "1.2.3.04", "example.123", "localhost", "internal", "foo.localhost", "bar.foo.localhost"]) {
    throws(() => defineHandler(value, {
      module: "./handlers/bad.ts",
      export: "bad",
      trigger: { kind: "input-changed", input: settings.id },
      inputs: [settings.id],
      egress: [host],
      outputs: [],
      source,
    }), "lowercase DNS name");
  }
  // A host whose last label is a real TLD is accepted even when an earlier
  // label happens to be all digits: only the *last* label decides "ends in
  // a number".
  defineHandler(value, {
    id: "numeric-label-ok",
    module: "./handlers/ok.ts",
    export: "ok",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id],
    egress: ["203.example.com"],
    outputs: [],
    source,
  });

  const selfOutput = defineHandlerOutput(value, { name: "self-flags", source });
  throws(() => defineHandler(value, {
    module: "./handlers/self.ts",
    export: "self",
    trigger: { kind: "input-changed", input: settings.id },
    inputs: [settings.id, selfOutput.id],
    egress: [],
    outputs: [selfOutput.outputId],
    source,
  }), "may not trigger on or read its own output");
}

{
  // Handler chaining mirrors compiler-core's handler-graph check: the graph
  // is acyclic before each `defineHandler` call, so the call that closes a
  // cycle is the one rejected, naming the cycle and its source location.
  const value = domain("handler-chain");
  const settings = value.observe(ConfigMap, { name: "settings", namespace: "workload", projection: ["data"], missing: "block-dependent-mutations", source });
  const raw = defineHandlerOutput(value, { name: "raw", source });
  const derived = defineHandlerOutput(value, { name: "derived", source });
  const summary = defineHandlerOutput(value, { name: "summary", source });
  const handler = (id: string, trigger: string, inputs: readonly string[], outputs: readonly string[], line = 1) => defineHandler(value, {
    id,
    module: `./handlers/${id}.ts`,
    export: "run",
    trigger: { kind: "input-changed", input: trigger },
    inputs,
    egress: [],
    outputs,
    source: { ...source, line },
  });
  handler("fetch-raw", settings.id, [settings.id, summary.id], [raw.outputId]);
  handler("derive", raw.id, [raw.id], [derived.outputId]);
  const chained = value.toIR().handlers.find((entry) => entry.id === "derive")!;
  equal(chained.trigger.input, raw.id, "a chained trigger lowers to the upstream output's Output-mode input");
  throws(() => handler("summarize", derived.id, [derived.id], [summary.outputId], 9), "closes a handler graph cycle through input-changed triggers: summarize -> fetch-raw -> derive -> summarize");
  throws(() => handler("summarize", derived.id, [derived.id], [summary.outputId], 9), `(${source.file}:9:${String(source.column)})`);
  const own = defineHandlerOutput(value, { name: "own", source });
  throws(() => handler("loop", own.id, [own.id], [own.outputId], 4), `may not trigger on or read its own output (${source.file}:4:${String(source.column)})`);
  handler("summarize", derived.id, [derived.id], []);
  equal(value.toIR().handlers.length, 3, "a rejected handler leaves no trace; the acyclic chain is kept");
}
