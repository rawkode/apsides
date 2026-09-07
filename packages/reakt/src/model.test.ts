import {
  ConfigMap,
  Deployment,
  NamespacedResource,
  Secret,
  Service,
  defineDomain,
  generateFixtureBundle,
  fixtureInputData,
  materializeFixtureIR,
  watch,
  type ContainerDefinition,
  type DomainBudgets,
  type SourceLocation,
} from "./model.ts";

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

function domain(id = "test") {
  return defineDomain({
    id,
    namespace: "workload",
    controlNamespace: "reakt-system",
    clusterBinding: "local-fixture",
    policyRef: "platform/test",
    enforcement: "api-bound",
    budgets,
    provenance: { inputs: [{ path: "domain.ts", sha256 }] },
  });
}

function deployment(id = "api", containers = [{ name: "api", image: "example.test/api@sha256:abc" }]) {
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
}

function deploymentWithEnv(env: NonNullable<ContainerDefinition["env"]>) {
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
}

function build(order: "service-first" | "service-last") {
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
}

const first = build("service-first");
const second = build("service-last");
const ir = first.toIR();
equal(JSON.stringify(ir), JSON.stringify(second.toIR()), "declaration order must not affect IR");
equal(ir.schemaVersion, "reakt.dev/ir/v1", "schema version");
equal(ir.testOnly, false, "author IR is production-marked");
equal(ir.resources.map((resource) => resource.id).join(","), "api,config,service", "resources sorted by stable ID");
equal(ir.inputs.length, 2, "managed and observed inputs emitted");
equal(ir.inputs.find((input) => input.kind === "Secret")?.sensitive, true, "Secret sensitivity retained");
equal(ir.reactions.length, 2, "rollout reactions emitted");
equal(ir.provenance.compiler, "reakt/0.1", "compiler provenance fixed by SDK");
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

const generated = generateFixtureBundle(ir, { workloadImage: "reakt-fixture:local" });
equal(generated.inputs.length, 1, "observed input fixture generated from IR");
equal(generated.workloadSubstitutions.length, 1, "Deployment fixture generated from IR");
equal(generated.inputs[0]?.data?.value, "synthetic-secret-observed-secret-credentials", "Secret fixture is synthetic and deterministic");
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
const secretBefore = JSON.stringify(secretIr);
const secretFixtures = generateFixtureBundle(secretIr, { workloadImage: "reakt-fixture:local" });
const secretFixture = secretFixtures.inputs[0]!;
equal(Object.keys(secretFixture.data ?? {}).sort().join(","), "uri,username,value", "all required Secret keys are seeded once without unrelated refs");
equal(secretFixture.data?.uri, secretFixture.data?.value, "required keys contain deterministic synthetic data");
equal(JSON.stringify(secretIr), secretBefore, "fixture completion leaves production IR unchanged");
throws(() => generateFixtureBundle(secretIr, {
  workloadImage: "reakt-fixture:local", inputs: [{ ...secretFixture, data: { value: "explicit" } }],
}), "missing required Secret key");
const explicitSeeds = generateFixtureBundle(secretIr, {
  workloadImage: "reakt-fixture:local", inputs: [{ ...secretFixture, data: { uri: "synthetic-uri", username: "synthetic-user" } }],
});
equal(explicitSeeds.inputs[0]?.data?.uri, "synthetic-uri", "complete explicit fixtures are preserved");
const otherNamespace = fixtureInputData(secretIr.inputs[0]!, secretIr.resources.map((resource) => ({ ...resource, namespace: "other" })));
equal(Object.keys(otherNamespace ?? {}).join(","), "value", "Secret refs in another namespace do not add keys");
const missingSeed = generateFixtureBundle(secretIr, {
  workloadImage: "reakt-fixture:local", inputs: [{ inputId: secretFixture.inputId, source, synthetic: true, state: "missing" }],
});
equal(missingSeed.inputs[0]?.state, "missing", "explicit missing fixture scenarios stay missing");
const fixtureIR = materializeFixtureIR(ir, generated);
equal(fixtureIR.testOnly, true, "fixture IR is test-only");
const fixtureDeployment = fixtureIR.resources.find((resource) => resource.id === "api")?.desired as unknown as { spec: { template: { spec: { containers: readonly { image: string; imagePullPolicy: string }[] } } } };
equal(fixtureDeployment.spec.template.spec.containers[0]?.image, "reakt-fixture:local", "fixture IR uses local image");
equal(fixtureDeployment.spec.template.spec.containers[0]?.imagePullPolicy, "Never", "fixture IR cannot pull externally");

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
  spec: { selector: { matchLabels: { app: "bad" } }, template: { metadata: { labels: { app: "bad" }, annotations: { "reakt.dev/revision": "forged" } }, spec: { containers: [{ name: "bad", image: "bad" }] } } },
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
  controlNamespace: "reakt-system",
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
  controlNamespace: "reakt-system",
  clusterBinding: "local-fixture",
  policyRef: "platform/test",
  enforcement: "api-bound",
  budgets: { ...budgets, maxResources: 1 },
  provenance: { inputs: [{ path: "domain.ts", sha256 }] },
});
constrained.manage(new ConfigMap({ metadata: { name: "one", namespace: "workload" } }), { source });
throws(() => constrained.manage(new ConfigMap({ metadata: { name: "two", namespace: "workload" } }), { source }), "maxResources");

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function equal(actual: unknown, expected: unknown, message: string): void {
  if (actual !== expected) throw new Error(`${message}: expected ${String(expected)}, received ${String(actual)}`);
}

function throws(run: () => unknown, expected: string): void {
  try {
    run();
  } catch (error) {
    assert(error instanceof Error && error.message.includes(expected), `expected error containing ${expected}, received ${String(error)}`);
    return;
  }
  throw new Error(`expected error containing ${expected}`);
}

function containsFunction(value: unknown, seen = new Set<object>()): boolean {
  if (typeof value === "function") return true;
  if (value === null || typeof value !== "object" || seen.has(value)) return false;
  seen.add(value);
  return Object.values(value).some((child) => containsFunction(child, seen));
}

{
  const value = defineDomain({
    id: "defaults", namespace: "workload", controlNamespace: "reakt-system",
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
