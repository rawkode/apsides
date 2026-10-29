import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { compileApsides } from "./compiler.ts";
import { buildFixtureOutputs } from "./fixtures.ts";

const root = await mkdtemp(join(tmpdir(), "apsides-source-tests-"));
const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
let sequence = 0;

const valid = `import { ConfigMap, Controller, Deployment, useResourceRef, useSecret } from "@apsides/kubernetes";
const Workload = () => {
  const config = useResourceRef("ConfigMap");
  const secret = useSecret({id:"secret",name:"secret"});
  return <>
    <ConfigMap id="config" ref={config} metadata={{name:"config",namespace:"workload"}} data={{mode:"test"}} />
    <Deployment id="api" metadata={{name:"api",namespace:"workload"}} rolloutOn={[config.data,secret.data]} spec={{selector:{matchLabels:{app:"api"}},template:{metadata:{labels:{app:"api"}},spec:{containers:[{name:"api",image:"example.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}}} />
  </>;
};
export default () => {
  return <Controller id="test" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}><Workload /></Controller>;
};
`;

type Dependency = {path:string;source:string;declared:boolean};
const plain = (message: string): string => message.replaceAll(/\x1b\[[0-9;]*m/g, "");
const rejectsWith = (operation: Promise<unknown>, diagnostic: RegExp, label: string, forbidden?: RegExp): Promise<void> => assert.rejects(operation, (error: unknown) => {
  assert.ok(error instanceof Error, `${label} must reject with an Error`);
  assert.match(plain(error.message), diagnostic, `${label} must be rejected with its diagnostic`);
  if (forbidden) assert.doesNotMatch(plain(error.message), forbidden, `${label} diagnostic must not match ${forbidden}`);
  return true;
});
const fixture = async (source: string, lockDigest = digest(source), dependencies: readonly Dependency[] = []): Promise<{sourcePath:string;output:string}> => {
  const id = sequence++;
  const directory = join(root, `case-${id}`);
  await import("node:fs/promises").then(fs => fs.mkdir(directory));
  const sourcePath = join(directory, "source.tsx");
  await writeFile(sourcePath, source);
  for (const dependency of dependencies) await writeFile(join(directory, dependency.path), dependency.source);
  const inputs = [{path:"source.tsx",sha256:lockDigest}, ...dependencies.filter(dependency => dependency.declared).map(dependency => ({path:dependency.path,sha256:digest(dependency.source)}))].sort((left, right) => left.path < right.path ? -1 : 1);
  const lock = { entry: "source.tsx", inputs, schemaVersion: "apsides.dev/source-lock/v1" };
  await writeFile(join(directory, "apsides.lock.json"), `${JSON.stringify(lock,null,2)}\n`);
  return {sourcePath,output:join(directory,"out")};
};

try {
  const first = await fixture(valid); const second = await fixture(valid);
  await compileApsides(first.sourcePath, first.output);
  await compileApsides(second.sourcePath, second.output);
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
  assert.equal(inputs.compilerClosure.schemaVersion, "apsides.dev/compiler-closure/v4");
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
  const consumed = buildFixtureOutputs(consumesSecret);
  assert.deepEqual(Object.keys(consumed.fixtures.inputs[0]?.data ?? {}).sort(), ["uri","username","value"]);
  assert.equal(consumed.fixtures.inputs[0]?.data?.uri, "synthetic-secret-secret");
  assert.equal(JSON.stringify(consumesSecret), beforeFixtures, "fixture seeds do not mutate production IR");
  consumer.namespace = "other";
  assert.deepEqual(Object.keys(buildFixtureOutputs(consumesSecret).fixtures.inputs[0]?.data ?? {}), ["value"]);
  const forced = structuredClone(production);
  forced.resources.find((resource: {id:string}) => resource.id === "api").force = true;
  const forcedCoverage = buildFixtureOutputs(forced).coverage;
  assert.ok(forcedCoverage.reactions.every(reaction => reaction.cases.some(entry => entry.scenario === "forced-field-conflict")));
  assert.ok(forcedCoverage.reactions.every(reaction => !reaction.cases.some(entry => entry.scenario === "field-conflict")));
  const mixed = structuredClone(production);
  mixed.reactions = [{ ...mixed.reactions[0], id: "mixed-rollout", inputs: ["managed-config", "secret"] }];
  const mixedCoverage = buildFixtureOutputs(mixed).coverage;
  assert.equal(mixedCoverage.summary.requiredCases, 21, "mixed reactions retain common, observed, managed, and mutation cases");
  const mixedReaction = mixedCoverage.reactions[0];
  assert.ok(mixedReaction);
  assert.deepEqual(mixedReaction.cases.find((entry: {scenario:string}) => entry.scenario === "initial-evaluation")?.applicability, {inputModes:["managed","observed"],inputIds:["managed-config","secret"],managedResourceIds:["config"]});
  assert.deepEqual(mixedReaction.cases.find((entry: {scenario:string}) => entry.scenario === "projected-change")?.applicability, {inputModes:["observed"],inputIds:["secret"],managedResourceIds:[]});
  assert.deepEqual(mixedReaction.cases.find((entry: {scenario:string}) => entry.scenario === "managed-drift-repair")?.applicability, {inputModes:["managed"],inputIds:["managed-config"],managedResourceIds:["config"]});
  const emptyManaged = structuredClone(production);
  emptyManaged.resources.find((resource: {id:string}) => resource.id === "config").desired.data = {};
  emptyManaged.reactions = [emptyManaged.reactions.find((reaction: {id:string}) => reaction.id === "rollout-managed-config-api")];
  const emptyManagedReaction = buildFixtureOutputs(emptyManaged).coverage.reactions[0];
  assert.ok(emptyManagedReaction);
  const emptyManagedCases = emptyManagedReaction.cases;
  assert.ok(!emptyManagedCases.some((entry: {scenario:string}) => entry.scenario === "managed-drift-repair"), "empty managed data has no owned value that can prove repair");
  assert.deepEqual(emptyManagedCases.filter((entry: {scenario:string}) => entry.scenario.startsWith("managed-")).map((entry: {scenario:string}) => entry.scenario), ["managed-missing","managed-read-denial","managed-unavailable"]);
  assert.ok(coverage.reactions.every((reaction: {source:{file:string}}) => reaction.source.file === "source.tsx"));
  assert.notEqual(overlay.productionIrSha256, overlay.fixtureIrSha256);

  const badLock = await fixture(valid, "0".repeat(64));
  await rejectsWith(compileApsides(badLock.sourcePath,badLock.output), /lock digest mismatch/, "lock digest mismatch");
  const typeError = await fixture(valid.replace("const Workload = () => {", "const Workload = () => {\n  const broken: string = 1;"));
  await rejectsWith(compileApsides(typeError.sourcePath,typeError.output), /TS2322 \[ERROR\]: Type 'number' is not assignable to type 'string'\.[\s\S]*source\.tsx:3:\d+/, "type error");
  // Spec section 28: undeclared evaluation input is rejected with a
  // source-related diagnostic. Evaluation-time denials name the capability
  // and fail inside the program's default export (source_default).
  const program = String.raw`at source_default \(`;
  const evaluation = (message: string): RegExp => new RegExp(String.raw`error: Uncaught \(in promise\) TypeError: ${message}\n(?:\s+at [^\n]*\n)*?\s+${program}`);
  for (const [name, needle, replacement, diagnostic] of [
    ["clock", "return <Controller", "Date.now(); return <Controller", evaluation(String.raw`secure mode Calling %SharedDate%\.now\(\) throws`)],
    ["random", "return <Controller", "Math.random(); return <Controller", evaluation(String.raw`secure mode %SharedMath%\.random\(\) throws`)],
    ["Deno ambient authority", "return <Controller", "Deno.env.get('HOME'); return <Controller", evaluation(String.raw`Cannot read properties of undefined \(reading 'env'\)`)],
    ["filesystem read", "return <Controller", "await Deno.readTextFile('/etc/hosts'); return <Controller", evaluation(String.raw`Cannot read properties of undefined \(reading 'readTextFile'\)`)],
    ["network access", "return <Controller", "await fetch('https://example.invalid/'); return <Controller", evaluation(String.raw`fetch is not a function`)],
    ["network socket", "return <Controller", "await Deno.connect({hostname:'127.0.0.1',port:9}); return <Controller", evaluation(String.raw`Cannot read properties of undefined \(reading 'connect'\)`)],
    ["dynamic module loading", "return <Controller", "await import('./other.ts'); return <Controller", /\[plugin apsides-locked-source-closure\]\nError: runtime imports are not declared build inputs/],
    ["WebAssembly", "return <Controller", "await WebAssembly.compile(new Uint8Array()); return <Controller", evaluation(String.raw`Cannot read properties of undefined \(reading 'compile'\)`)],
    ["CompressionStream", "return <Controller", "new CompressionStream('gzip'); return <Controller", evaluation(String.raw`CompressionStream is not a constructor`)],
    ["ReadableStream", "return <Controller", "new ReadableStream(); return <Controller", evaluation(String.raw`ReadableStream is not a constructor`)],
    ["constructor escape", "return <Controller", "({}).constructor.constructor('return globalThis')(); return <Controller", evaluation(String.raw`Function\.prototype\.constructor is not a valid constructor\.`)],
    ["prototype mutation", "return <Controller", "Array.prototype.push = () => 0; return <Controller", evaluation(String.raw`Cannot assign to read only property 'push' of 'root\.%ArrayPrototype%\.push'`)],
  ] as const) {
    const source = valid.replace("export default (", replacement.includes("await") ? "export default async (" : "export default (").replace(needle,replacement);
    const item = await fixture(source);
    await rejectsWith(compileApsides(item.sourcePath,item.output), diagnostic, name);
  }
  const dependency = {path:"dependency.ts",source:'export const declared = "present";\n'};
  const importsDependency = `import { declared } from "./dependency.ts";\nif (declared !== "present") throw new Error("dependency differs");\n${valid}`;
  const declared = await fixture(importsDependency, digest(importsDependency), [{...dependency,declared:true}]);
  await compileApsides(declared.sourcePath,declared.output);
  const undeclared = await fixture(importsDependency, digest(importsDependency), [{...dependency,declared:false}]);
  // The diagnostic names the missing module and the importing source line,
  // and the build stops at typecheck, before evaluation.
  await rejectsWith(compileApsides(undeclared.sourcePath,undeclared.output), /TS2307 \[ERROR\]: Cannot find module '[^'\n]*\/dependency\.ts'\.\n\s+at [^\n]*\/source\.tsx:1:\d+/, "undeclared import", /error: Uncaught/);
  // Handler chaining compiles end to end: a handler output's `.changed`
  // type-checks as an <On trigger> and the output itself as an <On inputs>
  // entry, and both lower to that output's Output-mode input.
  const chained = `import { Controller, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";
const Chain = () => {
  const settings = useConfigMap({ name: "settings" });
  const raw = useHandlerOutput({ id: "raw-flags", name: "raw-flags" });
  const derived = useHandlerOutput({ id: "derived", name: "derived" });
  return <>
    <On id="fetch-raw" trigger={settings.changed} handler="./a.ts#fetchRaw" inputs={[settings]} egress={["flags.example.com"]} outputs={[raw]} />
    <On id="derive" trigger={raw.changed} handler="./b.ts#derive" inputs={[raw]} outputs={[derived]} />
  </>;
};
export default () => {
  return <Controller id="chain" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}><Chain /></Controller>;
};
`;
  const chainModules = [
    {path:"a.ts",source:"export async function fetchRaw(): Promise<void> {}\n",declared:true},
    {path:"b.ts",source:"export async function derive(): Promise<void> {}\n",declared:true},
  ];
  const chain = await fixture(chained, digest(chained), chainModules);
  await compileApsides(chain.sourcePath,chain.output);
  const chainIR = JSON.parse(await readFile(join(chain.output,"ir.json"),"utf8"));
  const rawInput = chainIR.inputs.find((input: {mode:string;managedResourceId?:string}) => input.mode === "output" && input.managedResourceId === "raw-flags");
  assert.ok(rawInput, "the upstream output has an Output-mode input");
  const derive = chainIR.handlers.find((handler: {id:string}) => handler.id === "derive");
  assert.equal(derive.trigger.input, rawInput.id, "the downstream trigger is the upstream output's Output-mode input");
  assert.deepEqual(derive.inputs, [rawInput.id], "the downstream reads the upstream output");
  assert.deepEqual(derive.outputs, ["derived"]);
  assert.deepEqual(chainIR.handlers.find((handler: {id:string}) => handler.id === "fetch-raw").outputs, ["raw-flags"]);
  const looped = chained.replace("trigger={raw.changed} handler=\"./b.ts#derive\" inputs={[raw]}", "trigger={derived.changed} handler=\"./b.ts#derive\" inputs={[raw, derived]}");
  assert.notEqual(looped, chained, "the self-trigger rewrite applies");
  const loop = await fixture(looped, digest(looped), chainModules);
  await rejectsWith(compileApsides(loop.sourcePath,loop.output), /handler derive may not trigger on or read its own output \(source\.tsx:8:\d+\)/, "self-triggering handler");
  const tamed = await fixture(valid.replace("return <Controller", "if ('b'.localeCompare('a') <= 0 || new Error().stack !== '') throw new Error('unsafe SES taming'); return <Controller"));
  await compileApsides(tamed.sourcePath,tamed.output);
  console.log("Apsides source build passed: locked snapshots, Deno typecheck, declared-input closure and denied capabilities with their diagnostics, two-build/two-evaluation equality, source maps, generated fixtures, preservation diff, and scenario coverage.");
} finally { await rm(root,{recursive:true,force:true}); }
