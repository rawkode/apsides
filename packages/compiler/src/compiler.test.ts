import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { compileReakt } from "./compiler.ts";
import { buildFixtureOutputs } from "./fixtures.ts";

const root = await mkdtemp(join(tmpdir(), "reakt-source-tests-"));
const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
let sequence = 0;

const valid = `import { ConfigMap, Controller, Deployment, useResourceRef, useSecret } from "@reakt/kubernetes";
function Workload() {
  const config = useResourceRef("ConfigMap");
  const secret = useSecret({id:"secret",name:"secret"});
  return <>
    <ConfigMap id="config" ref={config} metadata={{name:"config",namespace:"workload"}} data={{mode:"test"}} />
    <Deployment id="api" metadata={{name:"api",namespace:"workload"}} rolloutOn={[config.data,secret.data]} spec={{selector:{matchLabels:{app:"api"}},template:{metadata:{labels:{app:"api"}},spec:{containers:[{name:"api",image:"example.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}}} />
  </>;
}
export default function build() {
  return <Controller id="test" namespace="workload" controlNamespace="reakt-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}><Workload /></Controller>;
}
`;

async function fixture(source: string, lockDigest = digest(source)): Promise<{sourcePath:string;output:string}> {
  const id = sequence++;
  const directory = join(root, `case-${id}`);
  await import("node:fs/promises").then(fs => fs.mkdir(directory));
  const sourcePath = join(directory, "source.tsx");
  await writeFile(sourcePath, source);
  const lock = { entry: "source.tsx", inputs: [{path:"source.tsx",sha256:lockDigest}], schemaVersion: "reakt.dev/source-lock/v1" };
  await writeFile(join(directory, "reakt.lock.json"), `${JSON.stringify(lock,null,2)}\n`);
  return {sourcePath,output:join(directory,"out")};
}

try {
  const first = await fixture(valid); const second = await fixture(valid);
  await compileReakt(first.sourcePath, first.output);
  await compileReakt(second.sourcePath, second.output);
  const files = (await readdir(first.output)).sort();
  assert.deepEqual(files, ["build-inputs.json","evaluate.mjs","fixture-catalogue.json","fixture-coverage.json","fixture-ir.json","fixture-overlay.json","fixtures.json","ir.json","program.mjs","program.mjs.map","source-map.json"]);
  for (const file of files) assert.equal(await readFile(join(first.output,file),"utf8"), await readFile(join(second.output,file),"utf8"), `${file} must be reproducible across output paths`);

  const inputs = JSON.parse(await readFile(join(first.output,"build-inputs.json"),"utf8"));
  assert.equal(inputs.reproducibility.observedEqual, true);
  assert.equal(inputs.reproducibility.constrainedInputProof, true);
  assert.equal(inputs.toolchain.ses.version, "2.3.0");
  assert.match(inputs.toolchain.ses.bundleSha256, /^[0-9a-f]{64}$/);
  assert.equal(inputs.evaluation.sandbox.engine, "SES");
  assert.deepEqual(inputs.evaluation.sandbox.endowments, ["buildContext"]);
  assert.equal(inputs.evaluation.sandbox.moduleLoader, false);
  assert.equal(inputs.evaluation.sandbox.lockdown.localeTaming, "safe");
  assert.equal(inputs.evaluation.sandbox.lockdown.errorTaming, "safe");
  assert.equal(inputs.compilerClosure.schemaVersion, "reakt.dev/compiler-closure/v2");
  assert.deepEqual(inputs.compilerClosure.authoringCompiler.sdk, inputs.sdk);
  assert.deepEqual(inputs.compilerClosure.authoringCompiler.toolchain, inputs.toolchain);
  assert.equal(inputs.compilerClosure.nativeCompiler, null, "native packaging must finalize the compiler closure");
  assert.match(inputs.compilerClosureDigest, /^sha256:[0-9a-f]{64}$/);
  assert.equal(inputs.bundleSha256, digest(await readFile(join(first.output,"program.mjs"),"utf8")));
  assert.equal(inputs.evaluationSourceSha256, digest(await readFile(join(first.output,"evaluate.mjs"),"utf8")));
  assert.notEqual(inputs.evaluationBundleSha256, digest(await readFile(join(first.output,"program.mjs"),"utf8")), "evaluation identity must not alias the emitted ESM bundle");
  assert.equal(inputs.evaluation.entryIdentity, "file:///$deno$stdin.mts");
  assert.equal(inputs.typecheck.status, "passed");
  assert.match(inputs.toolchain.deno.executableSha256, /^[0-9a-f]{64}$/);
  assert.match(inputs.sdk.sha256, /^[0-9a-f]{64}$/);

  const production = JSON.parse(await readFile(join(first.output,"ir.json"),"utf8"));
  const fixtureIr = JSON.parse(await readFile(join(first.output,"fixture-ir.json"),"utf8"));
  const fixtures = JSON.parse(await readFile(join(first.output,"fixtures.json"),"utf8"));
  const overlay = JSON.parse(await readFile(join(first.output,"fixture-overlay.json"),"utf8"));
  const coverage = JSON.parse(await readFile(join(first.output,"fixture-coverage.json"),"utf8"));
  assert.equal(production.testOnly, false);
  assert.equal(fixtureIr.testOnly, true);
  assert.equal(fixtures.productionReady, false);
  assert.equal(fixtures.inputs.length, 1, "observed Secret fixture is generated from IR");
  assert.equal(fixtures.inputs[0].data.value, "synthetic-secret-secret");
  assert.equal(fixtures.workloadSubstitutions[0].containers[0].imagePullPolicy, "Never");
  assert.equal(overlay.preservation.compilerVerified, true);
  assert.equal(coverage.summary.requiredCases, 29, "coverage contains only mode-applicable core scenarios plus one mutation check per reaction");
  const managedCoverage = coverage.reactions.find((reaction: {id:string}) => reaction.id === "rollout-managed-config-api");
  const observedCoverage = coverage.reactions.find((reaction: {id:string}) => reaction.id === "rollout-secret-api");
  assert.deepEqual(managedCoverage.cases.map((entry: {scenario:string}) => entry.scenario).sort(), ["accepted-write-restart","field-conflict","initial-evaluation","irrelevant-change","managed-drift-repair","managed-missing","managed-read-denial","managed-unavailable","out-of-scope-change","reaction-contribution-removed","rollout-readiness","same-value-replay"]);
  assert.ok(!managedCoverage.cases.some((entry: {scenario:string}) => entry.scenario === "projected-change"), "managed data drift must not be represented as an observed input change");
  assert.deepEqual(managedCoverage.cases.find((entry: {scenario:string}) => entry.scenario === "managed-drift-repair").applicability, {inputModes:["managed"],inputIds:["managed-config"],managedResourceIds:["config"]});
  assert.deepEqual(observedCoverage.cases.find((entry: {scenario:string}) => entry.scenario === "projected-change").applicability, {inputModes:["observed"],inputIds:["secret"],managedResourceIds:[]});
  const consumesSecret = structuredClone(production);
  const consumer = consumesSecret.resources.find((resource: {id:string}) => resource.id === "api");
  consumer.desired.spec.template.spec.containers = [
    {name:"api",image:"example.invalid/api",env:[{name:"URI",valueFrom:{secretKeyRef:{name:"secret",key:"uri"}}}]},
    {name:"sidecar",image:"example.invalid/sidecar",env:[{name:"USER",valueFrom:{secretKeyRef:{name:"secret",key:"username"}}}]},
  ];
  const beforeFixtures = JSON.stringify(consumesSecret);
  const consumed = buildFixtureOutputs(consumesSecret, undefined);
  assert.deepEqual(Object.keys(consumed.fixtures.inputs[0]?.data ?? {}).sort(), ["uri","username","value"]);
  assert.equal(consumed.fixtures.inputs[0]?.data?.uri, "synthetic-secret-secret");
  assert.equal(JSON.stringify(consumesSecret), beforeFixtures, "fixture seeds do not mutate production IR");
  const explicitFixture = {...consumed.fixtures.inputs[0], data:{value:"synthetic"}};
  assert.throws(() => buildFixtureOutputs(consumesSecret, {inputs:[explicitFixture]}), /missing required Secret key/);
  const complete = buildFixtureOutputs(consumesSecret, {inputs:[{...explicitFixture,data:{uri:"synthetic-uri",username:"synthetic-user"}}]});
  assert.equal(complete.fixtures.inputs[0]?.data?.uri, "synthetic-uri");
  assert.deepEqual(complete.fixtureIr, consumed.fixtureIr, "seed values do not alter the fixture IR");
  assert.deepEqual(complete.overlay, consumed.overlay, "seed values do not alter the overlay");
  assert.notDeepEqual(complete.coverage, consumed.coverage, "coverage binds the exact fixture seed values");
  consumer.namespace = "other";
  assert.deepEqual(Object.keys(buildFixtureOutputs(consumesSecret, undefined).fixtures.inputs[0]?.data ?? {}), ["value"]);
  const forced = structuredClone(production);
  forced.resources.find((resource: {id:string}) => resource.id === "api").force = true;
  const forcedCoverage = buildFixtureOutputs(forced, undefined).coverage;
  assert.ok(forcedCoverage.reactions.every(reaction => reaction.cases.some(entry => entry.scenario === "forced-field-conflict")));
  assert.ok(forcedCoverage.reactions.every(reaction => !reaction.cases.some(entry => entry.scenario === "field-conflict")));
  const mixed = structuredClone(production);
  mixed.reactions = [{ ...mixed.reactions[0], id: "mixed-rollout", inputs: ["managed-config", "secret"] }];
  const mixedCoverage = buildFixtureOutputs(mixed, undefined).coverage;
  assert.equal(mixedCoverage.summary.requiredCases, 21, "mixed reactions retain common, observed, managed, and mutation cases");
  const mixedReaction = mixedCoverage.reactions[0];
  assert.ok(mixedReaction);
  assert.deepEqual(mixedReaction.cases.find((entry: {scenario:string}) => entry.scenario === "initial-evaluation")?.applicability, {inputModes:["managed","observed"],inputIds:["managed-config","secret"],managedResourceIds:["config"]});
  assert.deepEqual(mixedReaction.cases.find((entry: {scenario:string}) => entry.scenario === "projected-change")?.applicability, {inputModes:["observed"],inputIds:["secret"],managedResourceIds:[]});
  assert.deepEqual(mixedReaction.cases.find((entry: {scenario:string}) => entry.scenario === "managed-drift-repair")?.applicability, {inputModes:["managed"],inputIds:["managed-config"],managedResourceIds:["config"]});
  const emptyManaged = structuredClone(production);
  emptyManaged.resources.find((resource: {id:string}) => resource.id === "config").desired.data = {};
  emptyManaged.reactions = [emptyManaged.reactions.find((reaction: {id:string}) => reaction.id === "rollout-managed-config-api")];
  const emptyManagedReaction = buildFixtureOutputs(emptyManaged, undefined).coverage.reactions[0];
  assert.ok(emptyManagedReaction);
  const emptyManagedCases = emptyManagedReaction.cases;
  assert.ok(!emptyManagedCases.some((entry: {scenario:string}) => entry.scenario === "managed-drift-repair"), "empty managed data has no owned value that can prove repair");
  assert.deepEqual(emptyManagedCases.filter((entry: {scenario:string}) => entry.scenario.startsWith("managed-")).map((entry: {scenario:string}) => entry.scenario), ["managed-missing","managed-read-denial","managed-unavailable"]);
  assert.ok(coverage.reactions.every((reaction: {source:{file:string}}) => reaction.source.file === "source.tsx"));
  assert.notEqual(overlay.productionIrSha256, overlay.fixtureIrSha256);

  const badLock = await fixture(valid, "0".repeat(64));
  await assert.rejects(compileReakt(badLock.sourcePath,badLock.output), /lock digest mismatch/);
  const typeError = await fixture(valid.replace("function Workload() {", "function Workload() {\n  const broken: string = 1;"));
  await assert.rejects(compileReakt(typeError.sourcePath,typeError.output), /deno failed/);
  for (const [_name, needle, replacement] of [
    ["clock", "return <Controller", "Date.now(); return <Controller"],
    ["random", "return <Controller", "Math.random(); return <Controller"],
    ["Deno ambient authority", "return <Controller", "Deno.env.get('HOME'); return <Controller"],
    ["dynamic module loading", "return <Controller", "await import('./other.ts'); return <Controller"],
    ["WebAssembly", "return <Controller", "await WebAssembly.compile(new Uint8Array()); return <Controller"],
    ["CompressionStream", "return <Controller", "new CompressionStream('gzip'); return <Controller"],
    ["ReadableStream", "return <Controller", "new ReadableStream(); return <Controller"],
    ["constructor escape", "return <Controller", "({}).constructor.constructor('return globalThis')(); return <Controller"],
    ["prototype mutation", "return <Controller", "Array.prototype.push = () => 0; return <Controller"],
  ] as const) {
    const source = valid.replace("export default function build", replacement.includes("await") ? "export default async function build" : "export default function build").replace(needle,replacement);
    const item = await fixture(source);
    await assert.rejects(compileReakt(item.sourcePath,item.output));
  }
  const tamed = await fixture(valid.replace("return <Controller", "if ('b'.localeCompare('a') <= 0 || new Error().stack !== '') throw new Error('unsafe SES taming'); return <Controller"));
  await compileReakt(tamed.sourcePath,tamed.output);
  console.log("Reakt source build passed: locked snapshots, Deno typecheck and denied capabilities, two-build/two-evaluation equality, source maps, generated fixtures, preservation diff, and scenario coverage.");
} finally { await rm(root,{recursive:true,force:true}); }
