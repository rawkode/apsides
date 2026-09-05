import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { compileReakt } from "./reakt.js";
import { buildFixtureOutputs } from "./reakt-fixtures.js";

const root = await mkdtemp(join(tmpdir(), "reakt-source-tests-"));
const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
let sequence = 0;

const valid = `import { ConfigMap, Deployment, Secret, defineDomain, watch } from "@reakt/kubernetes/legacy";
const source = {file:"source.ts",line:2,column:1};
export default function build(context: {sourceDigest:string;provenance:{inputs:{path:string;sha256:string}[]}}) {
  const domain = defineDomain({id:"test",namespace:"workload",controlNamespace:"reakt-system",clusterBinding:"fixture",policyRef:"platform/test",enforcement:"api-bound",budgets:{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2},provenance:context.provenance});
  const config = domain.manage(new ConfigMap({id:"config",metadata:{name:"config",namespace:"workload"},data:{mode:"test"}}),{source});
  const secret = domain.observe(Secret,{id:"secret",name:"secret",namespace:"workload",projection:["data"],missing:"block-dependent-mutations",source});
  const api = domain.manage(new Deployment({id:"api",metadata:{name:"api",namespace:"workload"},spec:{selector:{matchLabels:{app:"api"}},template:{metadata:{labels:{app:"api"}},spec:{containers:[{name:"api",image:"example.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}}}),{source});
  watch(config).project("data").rollout(api,{id:"config-rollout",source});
  watch(secret).project("data").rollout(api,{id:"secret-rollout",source});
  return domain;
}
`;

async function fixture(source: string, lockDigest = digest(source)): Promise<{sourcePath:string;output:string}> {
  const id = sequence++;
  const directory = join(root, `case-${id}`);
  await import("node:fs/promises").then(fs => fs.mkdir(directory));
  const sourcePath = join(directory, "source.ts");
  await writeFile(sourcePath, source);
  const lock = { entry: "source.ts", inputs: [{path:"source.ts",sha256:lockDigest}], schemaVersion: "reakt.dev/source-lock/v1" };
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
  const managedCoverage = coverage.reactions.find((reaction: {id:string}) => reaction.id === "config-rollout");
  const observedCoverage = coverage.reactions.find((reaction: {id:string}) => reaction.id === "secret-rollout");
  assert.deepEqual(managedCoverage.cases.map((entry: {scenario:string}) => entry.scenario).sort(), ["accepted-write-restart","field-conflict","initial-evaluation","irrelevant-change","managed-drift-repair","managed-missing","managed-read-denial","managed-unavailable","out-of-scope-change","reaction-contribution-removed","rollout-readiness","same-value-replay"]);
  assert.ok(!managedCoverage.cases.some((entry: {scenario:string}) => entry.scenario === "projected-change"), "managed data drift must not be represented as an observed input change");
  assert.deepEqual(managedCoverage.cases.find((entry: {scenario:string}) => entry.scenario === "managed-drift-repair").applicability, {inputModes:["managed"],inputIds:["managed-config"],managedResourceIds:["config"]});
  assert.deepEqual(observedCoverage.cases.find((entry: {scenario:string}) => entry.scenario === "projected-change").applicability, {inputModes:["observed"],inputIds:["secret"],managedResourceIds:[]});
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
  emptyManaged.reactions = [emptyManaged.reactions.find((reaction: {id:string}) => reaction.id === "config-rollout")];
  const emptyManagedReaction = buildFixtureOutputs(emptyManaged, undefined).coverage.reactions[0];
  assert.ok(emptyManagedReaction);
  const emptyManagedCases = emptyManagedReaction.cases;
  assert.ok(!emptyManagedCases.some((entry: {scenario:string}) => entry.scenario === "managed-drift-repair"), "empty managed data has no owned value that can prove repair");
  assert.deepEqual(emptyManagedCases.filter((entry: {scenario:string}) => entry.scenario.startsWith("managed-")).map((entry: {scenario:string}) => entry.scenario), ["managed-missing","managed-read-denial","managed-unavailable"]);
  assert.ok(coverage.reactions.every((reaction: {source:{file:string}}) => reaction.source.file === "source.ts"));
  assert.notEqual(overlay.productionIrSha256, overlay.fixtureIrSha256);

  const badLock = await fixture(valid, "0".repeat(64));
  await assert.rejects(compileReakt(badLock.sourcePath,badLock.output), /lock digest mismatch/);
  const typeError = await fixture(valid.replace("const source =", "const broken: string = 1;\nconst source ="));
  await assert.rejects(compileReakt(typeError.sourcePath,typeError.output), /deno failed/);
  for (const [_name, needle, replacement] of [
    ["clock", "return domain;", "Date.now(); return domain;"],
    ["random", "return domain;", "Math.random(); return domain;"],
    ["Deno ambient authority", "return domain;", "Deno.env.get('HOME'); return domain;"],
    ["dynamic module loading", "return domain;", "await import('./other.ts'); return domain;"],
    ["WebAssembly", "return domain;", "await WebAssembly.compile(new Uint8Array()); return domain;"],
    ["CompressionStream", "return domain;", "new CompressionStream('gzip'); return domain;"],
    ["ReadableStream", "return domain;", "new ReadableStream(); return domain;"],
    ["constructor escape", "return domain;", "({}).constructor.constructor('return globalThis')(); return domain;"],
    ["prototype mutation", "return domain;", "Array.prototype.push = () => 0; return domain;"],
  ] as const) {
    const source = valid.replace("export default function build", replacement.includes("await") ? "export default async function build" : "export default function build").replace(needle,replacement);
    const item = await fixture(source);
    await assert.rejects(compileReakt(item.sourcePath,item.output));
  }
  const tamed = await fixture(valid.replace("return domain;", "if ('b'.localeCompare('a') <= 0 || new Error().stack !== '') throw new Error('unsafe SES taming'); return domain;"));
  await compileReakt(tamed.sourcePath,tamed.output);
  const asyncLegacy = await fixture(valid.replace("export default function build", "export default async function build"));
  await compileReakt(asyncLegacy.sourcePath, asyncLegacy.output);
  assert.equal(JSON.parse(await readFile(join(asyncLegacy.output, "ir.json"), "utf8")).domain.id, "test", "legacy async default factories remain compatible");
  const prototypeKey = await fixture(valid.replace("return domain;", `return {productionIR:domain,fixtures:{inputs:[{inputId:"secret",source,synthetic:true,state:"present",data:JSON.parse('{"__proto__":"fixture"}')} ]}};`));
  await compileReakt(prototypeKey.sourcePath,prototypeKey.output);
  const prototypeFixtures = JSON.parse(await readFile(join(prototypeKey.output,"fixtures.json"),"utf8"));
  assert.equal(prototypeFixtures.inputs[0].data.__proto__, "fixture", "valid ConfigMap/Secret keys must survive fixture canonicalization");
  const badSource = await fixture(valid.replace('line:2', 'line:9999'));
  await assert.rejects(compileReakt(badSource.sourcePath,badSource.output), /invalid source location/);
  console.log("Reakt source build passed: locked snapshots, Deno typecheck and denied capabilities, two-build/two-evaluation equality, source maps, generated fixtures, preservation diff, and scenario coverage.");
} finally { await rm(root,{recursive:true,force:true}); }
