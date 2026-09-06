import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { compileReakt } from "./compiler.ts";

const digest = (value: string): string => createHash("sha256").update(value).digest("hex");
const root = await mkdtemp(join(tmpdir(), "reakt-tsx-tests-"));
let sequence = 0;

async function fixture(files: Readonly<Record<string, string>>, entry: string, denoConfig?: unknown): Promise<{ sourcePath: string; output: string }> {
  const directory = join(root, `case-${sequence++}`);
  await mkdir(directory);
  const entries = Object.entries(files).sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0);
  for (const [path, source] of entries) {
    const target = join(directory, path);
    await mkdir(dirname(target), { recursive: true });
    await writeFile(target, source);
  }
  const inputs = entries.map(([path, source]) => ({ path, sha256: digest(source) }));
  await writeFile(join(directory, "reakt.lock.json"), `${JSON.stringify({ entry, inputs, schemaVersion: "reakt.dev/source-lock/v1" }, null, 2)}\n`);
  if (denoConfig !== undefined) await writeFile(join(directory, "deno.json"), `${JSON.stringify(denoConfig, null, 2)}\n`);
  return { sourcePath: join(directory, entry), output: join(directory, "out") };
}

const program = `import { Controller } from "@reakt/kubernetes";
import { Api } from "./api.tsx";
export default (
  <Controller id="tsx" namespace="workload" controlNamespace="reakt-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}>
    <Api name="api" />
  </Controller>
);
`;

const api = `import { ConfigMap, Deployment, useConfigMap } from "@reakt/kubernetes";
export function Api({ name }: { readonly name: string }) {
  const settings = useConfigMap({ name: "settings" });
  return <>
    <ConfigMap metadata={{name: name + "-config",namespace:"workload"}} data={{mode:"test"}} />
    <Deployment metadata={{name,namespace:"workload"}} rolloutOn={[settings.data]} spec={{selector:{matchLabels:{app:name}},template:{metadata:{labels:{app:name}},spec:{containers:[{name,image:"example.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}}} />
  </>;
}
`;

try {
  const first = await fixture({ "api.tsx": api, "program.tsx": program }, "program.tsx");
  const second = await fixture({ "api.tsx": api, "program.tsx": program }, "program.tsx");
  await compileReakt(first.sourcePath, first.output);
  await compileReakt(second.sourcePath, second.output);

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
  assert.ok(bundleMap.sources.includes("reakt:/source/api.tsx"));
  assert.ok(bundleMap.sources.includes("reakt:/source/program.tsx"));

  const inputs = JSON.parse(await readFile(join(first.output, "build-inputs.json"), "utf8"));
  assert.deepEqual(inputs.sdk.files.map((file: { path: string }) => file.path), ["compiler.ts", "index.ts", "jsx-dev-runtime.ts", "jsx-runtime.ts", "model.ts", "runtime.ts"]);
  assert.deepEqual(inputs.toolchain.oxc.options.jsx, { development: true, importSource: "@reakt/kubernetes", runtime: "automatic" });
  assert.equal(inputs.toolchain.oxc.options.filename, "reakt:/source/<logical-path>");
  assert.deepEqual(inputs.typecheck.config, { importMap: "generated-local-sdk-only", jsx: "react-jsxdev", jsxImportSource: "@reakt/kubernetes" });

  const isolatedConfig = await fixture({ "api.tsx": api, "program.tsx": program }, "program.tsx", {
    compilerOptions: { jsx: "preserve", strict: false },
    imports: { "@reakt/kubernetes": "https://example.invalid/untrusted-sdk.ts" },
  });
  await compileReakt(isolatedConfig.sourcePath, isolatedConfig.output);
  assert.equal(JSON.parse(await readFile(join(isolatedConfig.output, "ir.json"), "utf8")).domain.id, "tsx", "author deno.json cannot alter the generated isolated typecheck config");

  const internal = await fixture({
    "program.ts": `import { renderExport } from "@reakt/kubernetes/compiler";\nexport default renderExport(null, {provenance:{inputs:[]}});\n`,
  }, "program.ts", { imports: { "@reakt/kubernetes/compiler": "./escape.ts" } });
  await assert.rejects(compileReakt(internal.sourcePath, internal.output), /deno failed/, "author config cannot expose the compiler-only SDK module");

  const remote = await fixture({ "program.ts": `import "https://deno.land/x/forbidden/mod.ts";\nexport default {};\n` }, "program.ts");
  await assert.rejects(compileReakt(remote.sourcePath, remote.output), /deno failed/, "remote imports remain disabled during typecheck");
  const npm = await fixture({ "program.ts": `import "npm:forbidden-package";\nexport default {};\n` }, "program.ts");
  await assert.rejects(compileReakt(npm.sourcePath, npm.output), /deno failed/, "npm imports remain disabled during typecheck");

  const asyncTsxSource = `import { Controller } from "@reakt/kubernetes";
export default async function build() {
  return <Controller id="async" namespace="workload" controlNamespace="reakt-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}} />;
}
`;
  const asyncTsx = await fixture({ "program.tsx": asyncTsxSource }, "program.tsx");
  await assert.rejects(compileReakt(asyncTsx.sourcePath, asyncTsx.output), "async TSX root factories must be rejected by synchronous rendering");

  for (const [name, source] of [
    ["raw IR", `export default {schemaVersion:"reakt.dev/ir/v1"};\n`],
    ["production IR wrapper", `export default {productionIR:{schemaVersion:"reakt.dev/ir/v1"},fixtures:null};\n`],
  ] as const) {
    const obsolete = await fixture({ "program.ts": source }, "program.ts");
    await assert.rejects(compileReakt(obsolete.sourcePath, obsolete.output), `${name} exports must not bypass the TSX renderer`);
  }

  console.log("Reakt TSX source build passed: development JSX lowering, multi-source reproducibility, source provenance, SDK closure, isolated Deno config, and synchronous rendering.");
} finally {
  await rm(root, { recursive: true, force: true });
}
