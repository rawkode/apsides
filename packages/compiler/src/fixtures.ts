import { createHash } from "node:crypto";
import { fixtureInputData } from "../../kubernetes/src/model.ts";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Source = { file: string; line: number; column: number };
type Resource = { force?: boolean; id: string; kind: string; namespace: string; source: Source; desired: Record<string, Json> };
type Input = { id: string; name: string; namespace: string; kind: "ConfigMap" | "Secret"; mode: "managed" | "observed"; managedResourceId?: string; sensitive: boolean; source: Source };
type Reaction = { id: string; op: "rollout"; inputs: string[]; target: string; source: Source };
export type ProductionIr = {
  schemaVersion: string;
  testOnly: boolean;
  domain: Record<string, Json>;
  resources: Resource[];
  inputs: Input[];
  reactions: Reaction[];
  provenance: { compiler: string; inputs: { path: string; sha256: string }[] };
};

type InputFixture = { inputId: string; source: Source; synthetic: true; state: "present"; data: Record<string, string> };
type WorkloadFixture = { targetResourceId: string; source: Source; containers: { name: string; image: string; imagePullPolicy: "Never" }[] };
export type FixtureBundle = {
  schemaVersion: "apsides.dev/fixtures/v1";
  testOnly: true;
  productionReady: false;
  inputs: InputFixture[];
  workloadSubstitutions: WorkloadFixture[];
};

const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
const order = (left: string, right: string): number => left < right ? -1 : left > right ? 1 : 0;

export const canonicalString = (value: unknown): string => {
  return JSON.stringify(canonical(value));
};

const FIXTURE_IMAGE = "apsides-fixture:local";

export const buildFixtureOutputs = (ir: ProductionIr) => {
  validateProductionIrShape(ir);
  const fixtures = completeFixtures(ir);
  const fixtureIr = materialize(ir, fixtures);
  const productionDigest = digest(canonicalString(ir));
  const fixtureDigest = digest(canonicalString(fixtureIr));
  if (productionDigest === fixtureDigest) throw new Error("fixture IR must have an identity distinct from production IR");
  const changes = diff(ir, fixtureIr);
  const allowed = /^\/resources\/\d+\/desired\/spec\/template\/spec\/containers\/\d+\/(image|imagePullPolicy)$/;
  for (const change of changes) {
    if (change.path !== "/testOnly" && !allowed.test(change.path)) throw new Error(`fixture overlay changes unsupported field ${change.path}`);
  }
  const overlay = {
    schemaVersion: "apsides.dev/fixture-overlay/v1",
    testOnly: true,
    productionReady: false,
    productionIrSha256: productionDigest,
    fixtureIrSha256: fixtureDigest,
    changes,
    preservation: {
      compilerVerified: true,
      preserved: ["domain", "resource-identities", "resource-lifecycle", "inputs", "reactions", "reaction-sources", "effect-vocabulary", "authorization-requirements"],
      allowedChanges: ["testOnly:false-to-true", "Deployment container image", "Deployment container imagePullPolicy"],
    },
    limitations: [
      "Fixture images establish primitive rollout and Pod readiness only; they do not establish application behavior.",
      "The v1 author schema rejects init containers, commands, probes, and mounts. Sidecars are ordinary containers and every declared container is substituted. Service accounts and resource requirements are preserved.",
    ],
  };
  const catalogue = fixtureCatalogue(FIXTURE_IMAGE);
  const coverage = fixtureCoverage(ir, fixtures, productionDigest, fixtureDigest, digest(canonicalString(fixtures)));
  return { fixtures, fixtureIr, overlay, catalogue, coverage };
};

const validateProductionIrShape = (ir: ProductionIr): void => {
  if (!plain(ir) || ir.schemaVersion !== "apsides.dev/ir/v1" || ir.testOnly !== false) throw new Error("source must emit production Apsides IR with testOnly false");
  if (!Array.isArray(ir.resources) || !Array.isArray(ir.inputs) || !Array.isArray(ir.reactions) || !plain(ir.provenance)) throw new Error("source emitted malformed Apsides IR arrays or provenance");
  const inputIds = new Set<string>();
  for (const input of ir.inputs) {
    if (!plain(input) || typeof input.id !== "string" || inputIds.has(input.id)) throw new Error("source emitted invalid or duplicate input identity");
    if (!(["ConfigMap", "Secret"] as unknown[]).includes(input.kind) || !(["managed", "observed"] as unknown[]).includes(input.mode)) throw new Error(`unsupported fixture input ${input.id}`);
    if (input.kind === "Secret" && (input.mode !== "observed" || input.sensitive !== true)) throw new Error(`Secret fixture contract lost sensitivity: ${input.id}`);
    if ((input.mode === "managed") !== (typeof input.managedResourceId === "string")) throw new Error(`fixture input ${input.id} has inconsistent managed resource identity`);
    inputIds.add(input.id);
  }
};

const completeFixtures = (ir: ProductionIr): FixtureBundle => {
  const inputs = ir.inputs.filter(input => input.mode === "observed").map(input => ({ inputId: input.id, source: input.source, synthetic: true as const, state: "present" as const, data: fixtureInputData(input, ir.resources) }));
  const workloadSubstitutions = ir.resources.filter(resource => resource.kind === "Deployment").map(resource => ({
    targetResourceId: resource.id,
    source: resource.source,
    containers: deploymentContainers(resource).map(container => ({ name: String(container.name), image: FIXTURE_IMAGE, imagePullPolicy: "Never" as const })),
  }));
  return canonical({ schemaVersion: "apsides.dev/fixtures/v1", testOnly: true, productionReady: false, inputs: inputs.sort((a,b) => order(a.inputId,b.inputId)), workloadSubstitutions: workloadSubstitutions.sort((a,b) => order(a.targetResourceId,b.targetResourceId)) }) as FixtureBundle;
};

const materialize = (ir: ProductionIr, fixtures: FixtureBundle): ProductionIr => {
  const substitutions = new Map(fixtures.workloadSubstitutions.map(item => [item.targetResourceId, item]));
  const resources = ir.resources.map(resource => {
    const substitution = substitutions.get(resource.id);
    if (!substitution) return resource;
    const desired = structuredClone(resource.desired);
    const containers = deploymentContainers({ ...resource, desired });
    const byName = new Map(substitution.containers.map(container => [container.name, container]));
    for (const container of containers) {
      const replacement = byName.get(String(container.name));
      if (!replacement) throw new Error(`missing container substitution ${resource.id}/${String(container.name)}`);
      container.image = replacement.image;
      container.imagePullPolicy = "Never";
    }
    return { ...resource, desired };
  });
  return canonical({ ...ir, testOnly: true, resources }) as ProductionIr;
};

const fixtureCatalogue = (image: string) => {
  return {
    schemaVersion: "apsides.dev/fixture-catalogue/v1",
    testOnly: true,
    image: { reference: image, requiredPullPolicy: "Never", sourceRecipe: "scripts/fixture-image", pinnedBase: "bash:5.2.37@sha256:3bee76a96d86d5d2d5efc7c1c570e5a7c95db22348a26944e0e546fa174e3324" },
    behaviors: [
      { id: "long-running", mode: "running", contract: "remains running and exits cleanly on TERM or INT" },
      { id: "exit-success", mode: "success", contract: "exits with status 0" },
      { id: "exit-failure", mode: "failure", contract: "exits with status 23" },
      { id: "controllable-readiness", mode: "readiness", contract: "writes /tmp/apsides-ready after APSIDES_READY_AFTER_SECONDS; use an exec probe for that path" },
    ],
  };
};

const forceScenario = ["forced-field-conflict", "api", "approved force reconciles competing fields while preserving object UID"] as const;

const commonScenarios = [
  ["initial-evaluation", "semantic", "available projected inputs produce a stable revision"],
  ["irrelevant-change", "semantic", "metadata outside the projection cannot affect revision"],
  ["same-value-replay", "semantic", "repeated equal input produces exactly equal revision"],
  ["accepted-write-restart", "workload", "accepted template revision is unchanged after restart"],
  ["field-conflict", "api", "competing field manager blocks without forced takeover"],
  ["out-of-scope-change", "api", "runtime credentials cannot mutate undeclared targets"],
  ["rollout-readiness", "workload", "real new-template Pods reach fixture readiness"],
] as const;

const observedScenarios = [
  ["projected-change", "semantic", "changed projected data changes the revision"],
  ["coalesced-inputs", "semantic", "evaluation uses the latest complete input set"],
  ["missing-input", "semantic", "missing required input blocks dependent mutation"],
  ["input-returned", "semantic", "returning required input recovers the stable revision"],
  ["unauthorized-input", "semantic", "typed unauthorized observation blocks dependent mutation; this is not API authorization evidence"],
  ["credential-read-denial", "api", "actual runtime credentials cannot read a declared input denied by policy and dependent mutation remains blocked"],
  ["unavailable-input", "semantic", "transport failure is distinguishable from absence"],
  ["invalid-input", "semantic", "invalid projection blocks without exposing contents"],
  ["restart-and-relist", "workload", "controller restart converges from changes made while stopped"],
] as const;

const managedDriftScenario = ["managed-drift-repair", "workload", "external managed-source data drift is repaired to declared intent without changing revision"] as const;
const managedScenarios = [
  ["managed-missing", "api", "missing managed source with denied repair blocks dependent mutation without changing revision"],
  ["managed-read-denial", "api", "actual runtime credentials denied managed-source reads block dependent mutation without changing revision"],
  ["managed-unavailable", "api", "unavailable managed-source reads block dependent mutation without changing revision"],
] as const;

const mutationScenario = ["reaction-contribution-removed", "mutation", "the primitive reference assertion fails when this reaction contribution is removed"] as const;

const fixtureCoverage = (ir: ProductionIr, fixtures: FixtureBundle, productionIrSha256: string, fixtureIrSha256: string, fixtureSha256: string) => {
  const inputs = new Map(ir.inputs.map(input => [input.id, input]));
  const resources = new Map(ir.resources.map(resource => [resource.id, resource]));
  let requiredCases = 0;
  const reactions = ir.reactions.map(reaction => {
    const reactionInputs = reaction.inputs.map(id => inputs.get(id)).filter((input): input is Input => input !== undefined);
    const observed = reactionInputs.filter(input => input.mode === "observed");
    const managed = reactionInputs.filter(input => input.mode === "managed");
    const managedWithOwnedData = managed.filter(input => {
      const data = input.managedResourceId === undefined ? undefined : resources.get(input.managedResourceId)?.desired.data;
      return plain(data) && Object.keys(data).length > 0;
    });
    const selected = [
      ...commonScenarios.map(scenario => ({ scenario: scenario[0] === "field-conflict" && resources.get(reaction.target)?.force === true ? forceScenario : scenario, applicable: reactionInputs })),
      ...(observed.length === 0 ? [] : observedScenarios.map(scenario => ({ scenario, applicable: observed }))),
      ...(managedWithOwnedData.length === 0 ? [] : [{ scenario: managedDriftScenario, applicable: managedWithOwnedData }]),
      ...(managed.length === 0 ? [] : managedScenarios.map(scenario => ({ scenario, applicable: managed }))),
      { scenario: mutationScenario, applicable: reactionInputs },
    ];
    requiredCases += selected.length;
    return {
      id: reaction.id, source: reaction.source, primitive: reaction.op, targetResourceId: reaction.target,
      inputs: reactionInputs.map(input => ({ id: input.id, kind: input.kind, mode: input.mode, sensitive: input.sensitive, ...(input.managedResourceId === undefined ? {} : { managedResourceId: input.managedResourceId }) })),
      cases: selected.map(({ scenario: [scenario, layer, assertion], applicable }) => ({
        id: `${reaction.id}:${scenario}`, reactionId: reaction.id, source: reaction.source, scenario, assertion, status: "required", layer, fixtureSha256, overlayIdentity: fixtureIrSha256,
        applicability: {
          inputModes: [...new Set(applicable.map(input => input.mode))].sort(order),
          inputIds: applicable.map(input => input.id).sort(order),
          managedResourceIds: applicable.flatMap(input => input.managedResourceId === undefined ? [] : [input.managedResourceId]).sort(order),
        },
        fixtureIds: applicable.filter(input => input.mode === "observed" && fixtures.inputs.some(item => item.inputId === input.id)).map(input => input.id).sort(order),
      })),
    };
  });
  return {
    schemaVersion: "apsides.dev/fixture-coverage/v1", testOnly: true, productionReady: false,
    productionIrSha256, fixtureIrSha256, fixtureSha256,
    artifactMode: "fixture-substituted",
    reactions,
    summary: { reactions: ir.reactions.length, requiredCases, generatedExternalInputs: fixtures.inputs.length, substitutedWorkloads: fixtures.workloadSubstitutions.length },
  };
};

const deploymentContainers = (resource: Resource): Record<string, Json>[] => {
  const spec = expectRecord(resource.desired.spec, `${resource.id}.spec`);
  const template = expectRecord(spec.template, `${resource.id}.spec.template`);
  const podSpec = expectRecord(template.spec, `${resource.id}.spec.template.spec`);
  return expectArray(podSpec.containers, `${resource.id} containers`).map((value, index) => expectRecord(value, `${resource.id} container ${index}`) as Record<string, Json>);
};

const diff = (before: unknown, after: unknown, path = ""): { op: "add" | "remove" | "replace"; path: string; before?: Json; after?: Json }[] => {
  if (canonicalString(before) === canonicalString(after)) return [];
  if (Array.isArray(before) && Array.isArray(after) && before.length === after.length) return before.flatMap((value, index) => diff(value, after[index], `${path}/${index}`));
  if (plain(before) && plain(after)) {
    const keys = [...new Set([...Object.keys(before), ...Object.keys(after)])].sort(order);
    return keys.flatMap(key => key in before && key in after ? diff(before[key], after[key], `${path}/${escapePointer(key)}`) : key in before ? [{ op: "remove" as const, path: `${path}/${escapePointer(key)}`, before: canonical(before[key]) }] : [{ op: "add" as const, path: `${path}/${escapePointer(key)}`, after: canonical(after[key]) }]);
  }
  return [{ op: "replace", path, before: canonical(before), after: canonical(after) }];
};

const canonical = (value: unknown): Json => {
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (Array.isArray(value)) return value.map(canonical);
  if (!plain(value)) throw new Error("fixture contract contains non-JSON data");
  const output: Record<string, Json> = Object.create(null);
  for (const key of Object.keys(value).sort(order)) output[key] = canonical(value[key]);
  return output;
};

const plain = (value: unknown): value is Record<string, unknown> => { return value !== null && typeof value === "object" && !Array.isArray(value) && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null); };
const expectRecord = (value: unknown, context: string): Record<string, any> => { if (!plain(value)) throw new Error(`${context} must be an object`); return value; };
const expectArray = (value: unknown, context: string): unknown[] => { if (!Array.isArray(value)) throw new Error(`${context} must be an array`); return value; };
const escapePointer = (value: string): string => { return value.replaceAll("~", "~0").replaceAll("/", "~1"); };
