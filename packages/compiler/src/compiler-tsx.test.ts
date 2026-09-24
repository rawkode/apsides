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
  assert.deepEqual(inputs.sdk.files.map((file: { path: string }) => file.path), ["compiler.ts", "index.ts", "jsx-dev-runtime.ts", "jsx-runtime.ts", "model.ts", "runtime.ts"]);
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
  const settings = useConfigMap({ name: "settings" });
  const flags = useHandlerOutput({ name: "flags" });
  return <On trigger={settings.changed} handler="./handlers/sync-flags.ts#syncFlags" inputs={[settings]} egress={["api.example.com"]} outputs={[flags]} />;
};
export default (
  <Controller id="handlers" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}>
    <HandlerApp />
  </Controller>
);
`;
  const handlerModule = `export async function syncFlags(event) {
  return event;
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
