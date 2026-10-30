import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { compileApsides } from "./compiler.ts";

const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
const root = await mkdtemp(join(tmpdir(), "apsides-tsx-tests-"));
let sequence = 0;

const fixture = async (files: Readonly<Record<string, string>>, entry: string, denoConfig?: unknown): Promise<{ sourcePath: string; output: string }> => {
  const directory = join(root, `case-${sequence++}`);
  await mkdir(directory);
  const entries = Object.entries(files).sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0);
  for (const [path, source] of entries) {
    const target = join(directory, path);
    await mkdir(dirname(target), { recursive: true });
    await writeFile(target, source);
  }
  const inputs = entries.map(([path, source]) => ({ path, sha256: digest(source) }));
  await writeFile(join(directory, "apsides.lock.json"), `${JSON.stringify({ entry, inputs, schemaVersion: "apsides.dev/source-lock/v1" }, null, 2)}\n`);
  if (denoConfig !== undefined) await writeFile(join(directory, "deno.json"), `${JSON.stringify(denoConfig, null, 2)}\n`);
  return { sourcePath: join(directory, entry), output: join(directory, "out") };
};

const program = `import { Controller } from "@apsides/kubernetes";
import { Api } from "./api.tsx";
export default (
  <Controller id="tsx" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}>
    <Api name="api" />
  </Controller>
);
`;

const api = `import { ConfigMap, Deployment, useConfigMap } from "@apsides/kubernetes";
export const Api = ({ name }: { readonly name: string }) => {
  const settings = useConfigMap({ name: "settings" });
  return <>
    <ConfigMap metadata={{name: name + "-config",namespace:"workload"}} data={{mode:"test"}} />
    <Deployment metadata={{name,namespace:"workload"}} rolloutOn={[settings.data]} spec={{selector:{matchLabels:{app:name}},template:{metadata:{labels:{app:name}},spec:{containers:[{name,image:"example.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}}} />
  </>;
};
`;

try {
  const first = await fixture({ "api.tsx": api, "program.tsx": program }, "program.tsx");
  const second = await fixture({ "api.tsx": api, "program.tsx": program }, "program.tsx");
  await compileApsides(first.sourcePath, first.output);
  await compileApsides(second.sourcePath, second.output);

  const files = (await readdir(first.output)).sort();
  for (const file of files) {
    assert.equal(await readFile(join(first.output, file), "utf8"), await readFile(join(second.output, file), "utf8"), `${file} must be reproducible for multi-source TSX`);
  }

  const ir = JSON.parse(await readFile(join(first.output, "ir.json"), "utf8"));
  assert.deepEqual(ir.provenance.inputs, [
    { path: "api.tsx", sha256: digest(api) },
    { path: "program.tsx", sha256: digest(program) },
  ]);
  assert.ok(ir.resources.every((resource: { source: { file: string } }) => resource.source.file === "api.tsx"));
  assert.equal(ir.inputs.find((input: { mode: string }) => input.mode === "observed")?.source.file, "program.tsx", "hooks inherit the custom component JSX invocation source");
  assert.ok(ir.reactions.every((reaction: { source: { file: string } }) => reaction.source.file === "api.tsx"));

  const declarationMap = JSON.parse(await readFile(join(first.output, "source-map.json"), "utf8"));
  for (const declaration of declarationMap.declarations) {
    assert.equal(declaration.inputSha256, digest(declaration.source.file === "api.tsx" ? api : program));
  }
  const bundleMap = JSON.parse(await readFile(join(first.output, "program.mjs.map"), "utf8"));
  assert.ok(bundleMap.sources.includes("apsides:/source/api.tsx"));
  assert.ok(bundleMap.sources.includes("apsides:/source/program.tsx"));

  const inputs = JSON.parse(await readFile(join(first.output, "build-inputs.json"), "utf8"));
  assert.deepEqual(inputs.sdk.files.map((file: { path: string }) => file.path), ["compiler.ts", "handler.ts", "index.ts", "jsx-dev-runtime.ts", "jsx-runtime.ts", "model.ts", "runtime.ts"]);
  assert.deepEqual(inputs.toolchain.oxc.options.jsx, { development: true, importSource: "@apsides/kubernetes", runtime: "automatic" });
  assert.equal(inputs.toolchain.oxc.options.filename, "apsides:/source/<logical-path>");
  assert.deepEqual(inputs.typecheck.config, { importMap: "generated-local-sdk-only", jsx: "react-jsxdev", jsxImportSource: "@apsides/kubernetes" });

  const isolatedConfig = await fixture({ "api.tsx": api, "program.tsx": program }, "program.tsx", {
    compilerOptions: { jsx: "preserve", strict: false },
    imports: { "@apsides/kubernetes": "https://example.invalid/untrusted-sdk.ts" },
  });
  await compileApsides(isolatedConfig.sourcePath, isolatedConfig.output);
  assert.equal(JSON.parse(await readFile(join(isolatedConfig.output, "ir.json"), "utf8")).domain.id, "tsx", "author deno.json cannot alter the generated isolated typecheck config");

  const internal = await fixture({
    "program.tsx": `import { renderExport } from "@apsides/kubernetes/compiler";\nexport default renderExport(null, {provenance:{inputs:[]}});\n`,
  }, "program.tsx", { imports: { "@apsides/kubernetes/compiler": "./escape.ts" } });
  await assert.rejects(compileApsides(internal.sourcePath, internal.output), /deno failed/, "author config cannot expose the compiler-only SDK module");

  const remote = await fixture({ "program.tsx": `import "https://deno.land/x/forbidden/mod.ts";\nexport default {};\n` }, "program.tsx");
  await assert.rejects(compileApsides(remote.sourcePath, remote.output), /deno failed/, "remote imports remain disabled during typecheck");
  const npm = await fixture({ "program.tsx": `import "npm:forbidden-package";\nexport default {};\n` }, "program.tsx");
  await assert.rejects(compileApsides(npm.sourcePath, npm.output), /deno failed/, "npm imports remain disabled during typecheck");

  const asyncTsxSource = `import { Controller } from "@apsides/kubernetes";
export default async () => {
  return <Controller id="async" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}} />;
};
`;
  const asyncTsx = await fixture({ "program.tsx": asyncTsxSource }, "program.tsx");
  await assert.rejects(compileApsides(asyncTsx.sourcePath, asyncTsx.output), "async TSX root factories must be rejected by synchronous rendering");

  const plainTs = await fixture({ "program.ts": `export default {};\n` }, "program.ts");
  await assert.rejects(compileApsides(plainTs.sourcePath, plainTs.output), /source entry must be a \.tsx program/, "TSX is the only authoring entry");

  // Neither case is about the schema version itself (both use the current
  // schema, "apsides.dev/ir/v2"): a plain-object export of already-compiled
  // IR, or an already-wrapped production-IR payload, must still be rejected
  // for bypassing the TSX renderer, regardless of the schema version it names.
  for (const [name, source] of [
    ["raw IR", `export default {schemaVersion:"apsides.dev/ir/v2"};\n`],
    ["production IR wrapper", `export default {productionIR:{schemaVersion:"apsides.dev/ir/v2"},fixtures:null};\n`],
  ] as const) {
    const obsolete = await fixture({ "program.tsx": source }, "program.tsx");
    await assert.rejects(compileApsides(obsolete.sourcePath, obsolete.output), `${name} exports must not bypass the TSX renderer`);
  }

  const handlerProgram = `import { Controller, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";
const HandlerApp = () => {
  const settings = useConfigMap({ id: "settings", name: "settings" });
  const flags = useHandlerOutput({ id: "flags", name: "flags" });
  return <On trigger={settings.changed} handler="./handlers/sync-flags.ts#syncFlags" inputs={[settings]} egress={["api.example.com"]} outputs={[flags]} />;
};
export default (
  <Controller id="handlers" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}>
    <HandlerApp />
  </Controller>
);
`;
  const handlerModule = `import type { HandlerEvent } from "@apsides/kubernetes/handler";

export async function syncFlags(event: HandlerEvent<"settings">) {
  const environment = event.inputs.settings.data.environment ?? "production";
  queueMicrotask(() => {});
  apsides.writeOutput("flags", "environment", environment);
  return event.id;
}
`;

  {
    // Bundle determinism: two independent compiles of the same program with
    // the same handler module produce byte-identical handlers/<id>.js and an
    // identical bundleSha256 in ir.json.
    const files = { "program.tsx": handlerProgram, "handlers/sync-flags.ts": handlerModule };
    const first = await fixture(files, "program.tsx");
    const second = await fixture(files, "program.tsx");
    await compileApsides(first.sourcePath, first.output);
    await compileApsides(second.sourcePath, second.output);

    const ir = JSON.parse(await readFile(join(first.output, "ir.json"), "utf8"));
    assert.equal(ir.handlers.length, 1, "one handler declared");
    const handler = ir.handlers[0];
    assert.equal(handler.module, "handlers/sync-flags.ts", "handler module is normalized");
    assert.equal(handler.export, "syncFlags", "handler export parsed from the module reference string");
    assert.match(handler.bundleSha256, /^[0-9a-f]{64}$/, "bundleSha256 is a lowercase sha256 digest");
    assert.equal(ir.outputs.length, 1, "one output declared");

    const bundlePath = join(first.output, "handlers", `${handler.id}.js`);
    const bundleCode = await readFile(bundlePath, "utf8");
    assert.equal(digest(bundleCode), handler.bundleSha256, "ir.json bundleSha256 matches the written handler bundle");
    assert.match(bundleCode, /syncFlags/, "the handler bundle contains the exported function");

    const secondBundleCode = await readFile(join(second.output, "handlers", `${handler.id}.js`), "utf8");
    assert.equal(bundleCode, secondBundleCode, "two independent compiles produce a byte-identical handler bundle");
    const secondIr = JSON.parse(await readFile(join(second.output, "ir.json"), "utf8"));
    assert.equal(secondIr.handlers[0].bundleSha256, handler.bundleSha256, "two independent compiles produce an identical bundleSha256");

    const buildInputs = JSON.parse(await readFile(join(first.output, "build-inputs.json"), "utf8"));
    assert.deepEqual(buildInputs.handlers, [{ id: handler.id, path: `handlers/${handler.id}.js`, sha256: handler.bundleSha256 }]);
  }

  {
    // Handler modules are type-checked against the ECMAScript library and
    // the SDK's handler declarations only, and the type-only SDK import
    // never reaches the bundle.
    const typed = await fixture({ "program.tsx": handlerProgram, "handlers/sync-flags.ts": handlerModule }, "program.tsx");
    await compileApsides(typed.sourcePath, typed.output);
    const ir = JSON.parse(await readFile(join(typed.output, "ir.json"), "utf8"));
    const bundle = await readFile(join(typed.output, "handlers", `${ir.handlers[0].id}.js`), "utf8");
    assert.doesNotMatch(bundle, /@apsides\/kubernetes/, "the type-only SDK import is erased from the handler bundle");
    const inputs = JSON.parse(await readFile(join(typed.output, "build-inputs.json"), "utf8"));
    assert.deepEqual(inputs.typecheck.handlers, { checkJs: true, declarations: "@apsides/kubernetes/handler", lib: ["esnext"], strict: true, unavailableGlobals: ["SharedArrayBuffer"], verbatimModuleSyntax: true, eventContracts: true, outputIds: true });

    const rejected: [string, string, string][] = [
      ["a type error", handlerModule.replace(`"environment", environment`, `"environment", 42`), "a wrong argument type fails the build"],
      ["a misspelled apsides member", handlerModule.replace("apsides.writeOutput(", "apsides.writeOutpt("), "an unknown apsides member fails the build"],
      ["a host global", handlerModule.replace("queueMicrotask(() => {});", "console.log(environment);"), "console does not exist in the engine"],
      ["a Deno global", handlerModule.replace("queueMicrotask(() => {});", "Deno.env.get(\"HOME\");"), "Deno does not exist in the engine"],
      ["an implicit any", "export async function syncFlags(event) { return event; }\n", "strict mode rejects an untyped event"],
      ["a type used through a value import", handlerModule.replace("import type { HandlerEvent }", "import { HandlerEvent }"), "verbatimModuleSyntax requires import type for types"],
      ["an input the <On> does not declare", `import type { HandlerEvent } from "@apsides/kubernetes/handler";\nexport async function syncFlags(event: HandlerEvent<"other">) { return event.inputs.other.data; }\n`, "the event contract rejects an undeclared input id"],
      ["an extra input id", `import type { HandlerEvent } from "@apsides/kubernetes/handler";\nexport async function syncFlags(event: HandlerEvent<"settings" | "extra">) { return event.inputs.settings.data; }\n`, "the event contract rejects an input id the handler is not given"],
      ["a missing export", handlerModule.replace("export async function syncFlags", "export async function syncFlag"), "the event contract requires the declared export"],
      ["an undeclared output id", handlerModule.replace(`writeOutput("flags"`, `writeOutput("other"`), "writeOutput accepts only the outputs the <On> declares"],
      ["a computed output id", handlerModule.replace(`writeOutput("flags"`, `writeOutput(environment`), "writeOutput rejects an id that is not a declared output's literal"],
    ];
    for (const [label, source, message] of rejected) {
      const broken = await fixture({ "program.tsx": handlerProgram, "handlers/sync-flags.ts": source }, "program.tsx");
      await assert.rejects(compileApsides(broken.sourcePath, broken.output), /deno failed/, `${message} (${label})`);
    }

    const undeclared = await fixture({ "program.tsx": handlerProgram, "handlers/sync-flags.ts": handlerModule.replace(`writeOutput("flags"`, `writeOutput("other"`) }, "program.tsx");
    await assert.rejects(compileApsides(undeclared.sourcePath, undeclared.output), /apsides\.writeOutput accepts flags\)/, "the failure names the handler's declared outputs");

    // JavaScript handlers get the same output-id check under checkJs: a
    // declared literal compiles; an undeclared literal, or an id widened to
    // string, fails.
    const jsProgram = handlerProgram.replace("./handlers/sync-flags.ts#syncFlags", "./handlers/sync-flags.js#syncFlags");
    const jsWriter = (statement: string) => `/** @param {import("@apsides/kubernetes/handler").HandlerEvent<"settings">} event */\nexport async function syncFlags(event) {\n  const environment = event.inputs.settings.data.environment ?? "production";\n  ${statement}\n}\n`;
    const jsValid = await fixture({ "program.tsx": jsProgram, "handlers/sync-flags.js": jsWriter(`apsides.writeOutput("flags", "environment", environment);`) }, "program.tsx");
    await compileApsides(jsValid.sourcePath, jsValid.output);
    for (const [label, statement] of [
      ["an undeclared literal", `apsides.writeOutput("other", "environment", environment);`],
      ["an id widened to string", `let id = "flags";\n  apsides.writeOutput(id, "environment", environment);`],
    ] as const) {
      const jsBroken = await fixture({ "program.tsx": jsProgram, "handlers/sync-flags.js": jsWriter(statement) }, "program.tsx");
      await assert.rejects(compileApsides(jsBroken.sourcePath, jsBroken.output), /apsides\.writeOutput accepts flags\)/, `a JavaScript handler's output id is checked (${label})`);
    }

    // A handler that declares no outputs cannot write one.
    const outputless = handlerProgram.replace(`  const flags = useHandlerOutput({ id: "flags", name: "flags" });\n`, "").replace(" outputs={[flags]}", "").replace("useConfigMap, useHandlerOutput", "useConfigMap");
    const noOutputs = await fixture({ "program.tsx": outputless, "handlers/sync-flags.ts": handlerModule }, "program.tsx");
    await assert.rejects(compileApsides(noOutputs.sourcePath, noOutputs.output), /apsides\.writeOutput accepts no outputs/, "a handler without outputs cannot call writeOutput");
    const readOnly = await fixture({ "program.tsx": outputless, "handlers/sync-flags.ts": handlerModule.replace(`  apsides.writeOutput("flags", "environment", environment);\n`, "") }, "program.tsx");
    await compileApsides(readOnly.sourcePath, readOnly.output);

    // Handlers with different outputs are checked separately: each module
    // may write its own handler's outputs. A module exporting both handlers
    // may write only the outputs they share.
    const twoHandlers = (first: string, second: string) => `import { Controller, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";
const HandlerApp = () => {
  const settings = useConfigMap({ id: "settings", name: "settings" });
  const flags = useHandlerOutput({ id: "flags", name: "flags" });
  const extra = useHandlerOutput({ id: "extra", name: "extra" });
  return <>
    <On id="sync-flags" trigger={settings.changed} handler="${first}" inputs={[settings]} outputs={[flags]} />
    <On id="sync-extra" trigger={settings.changed} handler="${second}" inputs={[settings]} outputs={[extra]} />
  </>;
};
export default (
  <Controller id="handlers" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}>
    <HandlerApp />
  </Controller>
);
`;
    const writer = (name: string, output: string) => `export async function ${name}(event: HandlerEvent<"settings">) { apsides.writeOutput(${JSON.stringify(output)}, "environment", event.inputs.settings.data.environment ?? "production"); }\n`;
    const header = `import type { HandlerEvent } from "@apsides/kubernetes/handler";\n`;
    const separate = await fixture({
      "program.tsx": twoHandlers("./handlers/sync-flags.ts#syncFlags", "./handlers/sync-extra.ts#syncExtra"),
      "handlers/sync-flags.ts": header + writer("syncFlags", "flags"),
      "handlers/sync-extra.ts": header + writer("syncExtra", "extra"),
    }, "program.tsx");
    await compileApsides(separate.sourcePath, separate.output);
    const swapped = await fixture({
      "program.tsx": twoHandlers("./handlers/sync-flags.ts#syncFlags", "./handlers/sync-extra.ts#syncExtra"),
      "handlers/sync-flags.ts": header + writer("syncFlags", "extra"),
      "handlers/sync-extra.ts": header + writer("syncExtra", "extra"),
    }, "program.tsx");
    await assert.rejects(compileApsides(swapped.sourcePath, swapped.output), /handler type check failed for sync-flags \(apsides\.writeOutput accepts flags\)/, "a handler cannot write another handler's output");
    const shared = await fixture({
      "program.tsx": twoHandlers("./handlers/sync.ts#syncFlags", "./handlers/sync.ts#syncExtra"),
      "handlers/sync.ts": header + writer("syncFlags", "flags") + writer("syncExtra", "extra"),
    }, "program.tsx");
    await assert.rejects(compileApsides(shared.sourcePath, shared.output), /deno failed/, "a module shared by handlers with different outputs writes only the outputs they share");
    // Imported modules are checked with each importing handler's outputs:
    // a module that writes one handler's output cannot be imported by
    // another, but a helper taking a HandlerOutputId parameter can.
    const crossImport = await fixture({
      "program.tsx": twoHandlers("./handlers/sync-flags.ts#syncFlags", "./handlers/sync-extra.ts#syncExtra"),
      "handlers/sync-flags.ts": `import { KEY } from "./sync-extra.ts";\n` + header + writer("syncFlags", "flags").replace(`"environment"`, "KEY"),
      "handlers/sync-extra.ts": header + `export const KEY = "environment";\n` + writer("syncExtra", "extra"),
    }, "program.tsx");
    await assert.rejects(compileApsides(crossImport.sourcePath, crossImport.output), /handler type check failed for sync-flags \(apsides\.writeOutput accepts flags\)/, "an imported module is checked with the importing handler's outputs");
    const helper = `import type { HandlerOutputId } from "@apsides/kubernetes/handler";\nexport function write(id: HandlerOutputId, value: string): void { apsides.writeOutput(id, "environment", value); }\n`;
    const helperWriter = (name: string, output: string) => `import { write } from "./write.ts";\n${header}export async function ${name}(event: HandlerEvent<"settings">) { write(${JSON.stringify(output)}, event.inputs.settings.data.environment ?? "production"); }\n`;
    const sharedHelper = await fixture({
      "program.tsx": twoHandlers("./handlers/sync-flags.ts#syncFlags", "./handlers/sync-extra.ts#syncExtra"),
      "handlers/sync-flags.ts": helperWriter("syncFlags", "flags"),
      "handlers/sync-extra.ts": helperWriter("syncExtra", "extra"),
      "handlers/write.ts": helper,
    }, "program.tsx");
    await compileApsides(sharedHelper.sourcePath, sharedHelper.output);
    const helperMisuse = await fixture({
      "program.tsx": twoHandlers("./handlers/sync-flags.ts#syncFlags", "./handlers/sync-extra.ts#syncExtra"),
      "handlers/sync-flags.ts": helperWriter("syncFlags", "extra"),
      "handlers/sync-extra.ts": helperWriter("syncExtra", "extra"),
      "handlers/write.ts": helper,
    }, "program.tsx");
    await assert.rejects(compileApsides(helperMisuse.sourcePath, helperMisuse.output), /handler type check failed for sync-flags/, "a shared helper still rejects another handler's output id");

    // A locked module that only supplies types to a handler is reachable
    // through its erased import type, not an unreachable lock entry.
    const typesOnly = await fixture({
      "program.tsx": handlerProgram,
      "handlers/sync-flags.ts": `import type { Environment } from "./types.ts";\nimport type { HandlerEvent } from "@apsides/kubernetes/handler";\nexport async function syncFlags(event: HandlerEvent<"settings">): Promise<Environment> { return event.inputs.settings.data.environment ?? "production"; }\n`,
      "handlers/types.ts": `export type Environment = string;\nexport type Names = { SharedArrayBuffer: string };\n`,
    }, "program.tsx");
    await compileApsides(typesOnly.sourcePath, typesOnly.output);

    // SharedArrayBuffer type-checks under lib esnext but the engine does
    // not install it; a reference to it, directly or through globalThis,
    // fails the build. A property named SharedArrayBuffer (above) does not.
    for (const reference of ["new SharedArrayBuffer(8);", "globalThis.SharedArrayBuffer;"]) {
      const shared = await fixture({ "program.tsx": handlerProgram, "handlers/sync-flags.ts": handlerModule.replace("queueMicrotask(() => {});", reference) }, "program.tsx");
      await assert.rejects(compileApsides(shared.sourcePath, shared.output), /uses SharedArrayBuffer, which the handler engine does not provide/, `SharedArrayBuffer is rejected (${reference})`);
    }

    // JavaScript is checked too: a .js helper a handler imports cannot use
    // a global the engine does not have.
    const jsHelper = await fixture({
      "program.tsx": handlerProgram,
      "handlers/sync-flags.ts": `import { describe } from "./describe.js";\nimport type { HandlerEvent } from "@apsides/kubernetes/handler";\nexport async function syncFlags(event: HandlerEvent<"settings">) { return describe(event.id); }\n`,
      "handlers/describe.js": `export function describe(id) { console.log(id); return id; }\n`,
    }, "program.tsx");
    await assert.rejects(compileApsides(jsHelper.sourcePath, jsHelper.output), /deno failed/, "a JavaScript helper using console fails the handler check");

    const valueImport = await fixture({
      "program.tsx": handlerProgram,
      "handlers/sync-flags.ts": `import { HandlerEvent } from "@apsides/kubernetes/handler";\nexport async function syncFlags(event: HandlerEvent) { return [event.id, HandlerEvent]; }\n`,
    }, "program.tsx");
    await assert.rejects(compileApsides(valueImport.sourcePath, valueImport.output), /deno failed|undeclared package/, "a value import of the SDK cannot reach a handler bundle");
  }

  {
    // A handler module absent from apsides.lock.json is rejected: it is
    // never bundled from the filesystem outside the locked closure.
    const unlockedProgram = handlerProgram.replace("./handlers/sync-flags.ts", "./handlers/unlocked.ts");
    const unlocked = await fixture({ "program.tsx": unlockedProgram, "handlers/sync-flags.ts": handlerModule }, "program.tsx");
    await assert.rejects(
      compileApsides(unlocked.sourcePath, unlocked.output),
      /handler module is absent from the source lock/,
      "an unlocked handler module must be rejected before bundling",
    );
  }

  console.log("Apsides TSX source build passed: development JSX lowering, multi-source reproducibility, source provenance, SDK closure, isolated Deno config, and synchronous rendering.");
} finally {
  await rm(root, { recursive: true, force: true });
}
