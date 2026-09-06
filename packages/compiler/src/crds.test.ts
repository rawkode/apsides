import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import * as ts from "typescript";
import { generateCrdTypes } from "./crds.ts";

const fixture = {
  apiVersion: "apiextensions.k8s.io/v1", kind: "CustomResourceDefinition",
  spec: { group: "postgresql.cnpg.io", scope: "Namespaced", names: { kind: "Cluster", plural: "clusters" },
    versions: [{ name: "v1", served: true, storage: true, schema: { openAPIV3Schema: {
      type: "object", properties: { spec: { type: "object", required: ["instances"],
        properties: { instances: { type: "integer" }, storage: { type: "object", properties: { size: { type: "string" } } } },
      } },
    } } }],
  },
};
const generated = await generateCrdTypes(JSON.stringify(fixture), "Cluster", "v1");
assert.match(generated, /instances: number/);
assert.match(generated, /size\?: string/);
assert.match(generated, /defineNamespacedResource<ClusterSpec>/);
assert.match(generated, /"plural": "clusters"/);
await assert.rejects(generateCrdTypes(JSON.stringify({ ...fixture, spec: { ...fixture.spec, scope: "Cluster" } }), "Cluster"));
await assert.rejects(generateCrdTypes(JSON.stringify(fixture), "Cluster", "v2"));
await assert.rejects(generateCrdTypes(`${JSON.stringify(fixture)}\n---\n${JSON.stringify(fixture)}`, "Cluster"));
const unsafe = structuredClone(fixture);
Object.assign(unsafe.spec.versions[0]?.schema.openAPIV3Schema.properties.spec ?? {}, { tsType: "arbitrary code" });
await assert.rejects(generateCrdTypes(JSON.stringify(unsafe), "Cluster"));

const project = await mkdtemp(join(tmpdir(), "aps-crd-language-service-"));
try {
  const generatedPath = join(project, "cnpg.ts");
  const validPath = join(project, "valid.tsx");
  const completionPath = join(project, "completion.tsx");
  const completionMarker = "/* complete */";
  await writeFile(generatedPath, generated);
  await writeFile(validPath, `import { Cluster } from "./cnpg.ts";
export default <Cluster metadata={{ name: "database", namespace: "default" }} spec={{ instances: 1, storage: { size: "1Gi" } }} />;
`);
  const completionSource = `import { Cluster } from "./cnpg.ts";
export default <Cluster metadata={{ name: "database", namespace: "default" }} spec={{ ${completionMarker} }} />;
`;
  await writeFile(completionPath, completionSource);

  const sdk = fileURLToPath(new URL("../../reakt/src", import.meta.url));
  const compilerOptions: ts.CompilerOptions = {
    allowImportingTsExtensions: true,
    baseUrl: project,
    jsx: ts.JsxEmit.ReactJSX,
    jsxImportSource: "@reakt/kubernetes",
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    noEmit: true,
    paths: {
      "@reakt/kubernetes": [join(sdk, "index.ts")],
      "@reakt/kubernetes/jsx-runtime": [join(sdk, "jsx-runtime.ts")],
    },
    skipLibCheck: true,
    strict: true,
    target: ts.ScriptTarget.ES2022,
  };
  const roots = [generatedPath, validPath, completionPath];
  const service = ts.createLanguageService({
    fileExists: ts.sys.fileExists,
    getCompilationSettings: () => compilerOptions,
    getCurrentDirectory: () => project,
    getDefaultLibFileName: options => ts.getDefaultLibFilePath(options),
    getDirectories: ts.sys.getDirectories,
    getNewLine: () => ts.sys.newLine,
    getScriptFileNames: () => roots,
    getScriptSnapshot: path => {
      const source = ts.sys.readFile(path);
      return source === undefined ? undefined : ts.ScriptSnapshot.fromString(source);
    },
    getScriptVersion: () => "0",
    readDirectory: ts.sys.readDirectory,
    readFile: ts.sys.readFile,
    useCaseSensitiveFileNames: () => ts.sys.useCaseSensitiveFileNames,
  });
  const diagnostics = [generatedPath, validPath].flatMap(path => [
    ...service.getSyntacticDiagnostics(path),
    ...service.getSemanticDiagnostics(path),
  ]);
  assert.deepEqual(diagnostics, [], diagnostics.map(diagnostic => ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n")).join("\n"));
  const completion = service.getCompletionsAtPosition(completionPath, completionSource.indexOf(completionMarker), {});
  const names = new Set(completion?.entries.map(entry => entry.name));
  assert(names.has("instances"), "Cluster spec completion must include instances");
  assert(names.has("storage"), "Cluster spec completion must include storage");
} finally {
  await rm(project, { recursive: true, force: true });
}
console.log("CRD type generation and rejection checks passed");
