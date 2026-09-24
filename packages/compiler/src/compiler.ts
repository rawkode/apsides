import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { lstat, mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { dirname, join, posix, relative, resolve, sep } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";
import { transform } from "oxc-transform";
import { rolldown } from "rolldown";
import { parseAst } from "rolldown/parseAst";
import { buildFixtureOutputs, canonicalString, type ProductionIr } from "./fixtures.ts";

const OXC_VERSION = "0.58.0";
const ROLLDOWN_VERSION = "1.0.0-beta.35";
const SES_VERSION = "2.3.0";
export const SOURCE_LOCK_SCHEMA = "apsides.dev/source-lock/v1";
export const COMPILER_CLOSURE_SCHEMA = "apsides.dev/compiler-closure/v3";
const MAX_MODULES = 128;
const MAX_MODULE_BYTES = 1024 * 1024;
const MAX_OUTPUT_BYTES = 8 * 1024 * 1024;
const executionEnvironment = Object.freeze({ TZ: "UTC", LANG: "C", LC_ALL: "C" });
const SDK_PACKAGE = "@apsides/kubernetes";
const SDK_FILES = ["index.ts", "model.ts", "runtime.ts", "compiler.ts", "jsx-runtime.ts", "jsx-dev-runtime.ts"] as const;
const SDK_PUBLIC_IMPORTS = new Map<string, string>([
  [SDK_PACKAGE, "index.ts"],
  [`${SDK_PACKAGE}/jsx-runtime`, "jsx-runtime.ts"],
  [`${SDK_PACKAGE}/jsx-dev-runtime`, "jsx-dev-runtime.ts"],
]);
const SDK_COMPILER_IMPORT = `${SDK_PACKAGE}/compiler`;
const GENERATED_ENTRY = "apsides:/compiler/entry.ts";
const sha256 = (value: string | Uint8Array): string => createHash("sha256").update(value).digest("hex");
const order = (left: string, right: string): number => left < right ? -1 : left > right ? 1 : 0;
const within = (root: string, path: string): boolean => path === root || path.startsWith(`${root}${sep}`);

interface SourceLock {
  schemaVersion: typeof SOURCE_LOCK_SCHEMA;
  entry: string;
  inputs: { path: string; sha256: string }[];
}

interface Snapshot {
  logical: string;
  virtual: string;
  physical: string;
  text: string;
  sha256: string;
}

export interface CompilerClosure {
  readonly schemaVersion: typeof COMPILER_CLOSURE_SCHEMA;
  readonly authoringCompiler: { readonly sdk: unknown; readonly toolchain: unknown };
  readonly nativeCompiler: null | {
    readonly builderImage: { readonly reference: string; readonly platform: "linux/amd64" | "linux/arm64" };
    readonly cargoLockSha256: string;
    readonly buildContextFiles: readonly { readonly path: string; readonly sha256: string }[];
    readonly packagingFiles: readonly { readonly path: string; readonly sha256: string }[];
    readonly buildProfileSha256: string;
    readonly hostTools: readonly { readonly role: string; readonly sha256: string; readonly version: string }[];
    readonly trustedCliCodeSha256: string;
    readonly approvedRuntime: unknown;
    readonly rustyV8: unknown;
  };
}

/** Canonical identity used by the release verifier after packaging fills nativeCompiler. */
export const compilerClosureDigest = (closure: CompilerClosure): string => {
  return `sha256:${sha256(canonicalString(closure))}`;
};

/** Compile a digest-locked source closure and evaluate it twice in a locked-down SES Compartment hosted by Deno. */
export const compileApsides = async (sourceFile: string, output: string): Promise<void> => {
  const source = await realpath(sourceFile);
  const sourceRoot = dirname(source);
  const destination = resolve(output);
  if (await exists(destination)) throw new Error("source output already exists; choose a fresh directory");
  const lockPath = await realpath(join(sourceRoot, "apsides.lock.json"));
  if (!within(sourceRoot, lockPath)) throw new Error("source lock must be inside the source root");
  const lockBytes = await readFile(lockPath, "utf8");
  const lock = parseCanonicalLock(lockBytes);
  const entryLogical = normalizeLogical(relative(sourceRoot, source), "source entry");
  if (!entryLogical.endsWith(".tsx")) throw new Error("source entry must be a .tsx program");
  if (lock.entry !== entryLogical) throw new Error(`source lock entry ${lock.entry} does not match ${entryLogical}`);

  // Both builds consume this immutable in-memory snapshot, captured before parsing or evaluation.
  const author = new Map<string, Snapshot>();
  for (const input of lock.inputs) {
    const physical = resolve(sourceRoot, input.path);
    if (!within(sourceRoot, physical)) throw new Error(`source lock input escapes root: ${input.path}`);
    const actual = await realpath(physical);
    if (actual !== physical || !(await lstat(physical)).isFile()) throw new Error(`source input must be a regular non-symlink file: ${input.path}`);
    const bytes = await readFile(physical);
    if (bytes.byteLength > MAX_MODULE_BYTES) throw new Error(`source module exceeds 1 MiB budget: ${input.path}`);
    const actualDigest = sha256(bytes);
    if (actualDigest !== input.sha256) throw new Error(`source lock digest mismatch: ${input.path}`);
    author.set(input.path, { logical: input.path, virtual: `apsides:/source/${input.path}`, physical, text: bytes.toString("utf8"), sha256: actualDigest });
  }
  if (author.size > MAX_MODULES) throw new Error(`source lock exceeds ${MAX_MODULES} module budget`);
  if (!author.has(entryLogical)) throw new Error("source lock does not include its entry");

  const sdk = await snapshotSdk();
  const sesRoot = await findPackageRoot(fileURLToPath(import.meta.resolve("ses")));
  const sesPath = join(sesRoot, "dist", "ses.umd.min.js");
  const sesBytes = await readFile(sesPath);
  const deno = await realpath(process.execPath);
  const denoBytes = await readFile(deno);
  const denoVersion = (await run(deno, ["--version"], "", { ...executionEnvironment })).stdout.trim();
  // Capture the complete compiler/tool receipt before any author typecheck,
  // transform, bundle, or evaluation consumes those installed bytes.
  const compilerFiles = await compilerIdentity();
  const installedTools = await installedToolIdentity();

  await typecheckSnapshot(author, sdk, entryLogical, deno);
  const first = await bundleSnapshot(author, sdk, entryLogical);
  const second = await bundleSnapshot(author, sdk, entryLogical);
  const firstMap = normalizeSourceMap(first.map);
  const secondMap = normalizeSourceMap(second.map);
  if (first.bundle !== second.bundle || first.evaluationBundle !== second.evaluationBundle || canonicalString(firstMap) !== canonicalString(secondMap)) throw new Error("independent Oxc/Rolldown builds were not byte-identical");
  if (Buffer.byteLength(first.bundle) > MAX_OUTPUT_BYTES) throw new Error("closed author bundle exceeds 8 MiB budget");

  const provenanceInputs = [...author.values()].map(({ logical: path, sha256 }) => ({ path, sha256 })).sort((a,b) => order(a.path,b.path));
  const buildContext = { sourceDigest: author.get(entryLogical)!.sha256, provenance: { inputs: provenanceInputs } };
  const evaluateSource = evaluator(sesBytes.toString("utf8"), first.evaluationBundle, buildContext);
  const evaluationArgs = denoEvaluationArgs();
  const runOne = await run(deno, evaluationArgs, evaluateSource, { ...executionEnvironment });
  const runTwo = await run(deno, evaluationArgs, evaluateSource, { ...executionEnvironment });
  if (runOne.stderr !== runTwo.stderr || runOne.stdout !== runTwo.stdout) throw new Error("isolated source evaluations were not byte-identical");
  const evaluated = parseEvaluation(runOne.stdout);
  const ir = evaluated.productionIR;
  ir.provenance = { compiler: "apsides/0.1", inputs: provenanceInputs };
  validateSourceLocations(ir, author);
  const handlerBundles = await bundleHandlers(author, ir);
  ir.handlers = handlerBundles.handlers;
  const reachable = new Set([...first.loaded, ...handlerBundles.loaded]);
  for (const item of author.values()) if (!reachable.has(item.virtual)) throw new Error(`source lock contains unreachable module: ${item.logical}`);
  const fixture = buildFixtureOutputs(ir);
  validateSourceLocations(fixture.fixtureIr, author);

  const bundleMap = firstMap;
  const declarationMap = buildDeclarationMap(ir, fixture.fixtures, author);
  const emittedProgram = `${first.bundle}\n//# sourceMappingURL=program.mjs.map\n`;
  const sdkFiles = [...sdk.values()].map(({ logical: path, sha256 }) => ({ path, sha256 })).sort((a,b) => order(a.path,b.path));
  const sdkIdentity = { identity: SDK_PACKAGE, version: "0.1.0", files: sdkFiles, sha256: sha256(canonicalString(sdkFiles)) };
  const toolchain = {
    deno: { pathIdentity: "deno", version: denoVersion, target: Deno.build.target, platform: process.platform, arch: process.arch, executableSha256: sha256(denoBytes) },
    oxc: { version: OXC_VERSION, options: { lang: "per-extension", sourcemap: true, jsx: { runtime: "automatic", development: true, importSource: SDK_PACKAGE }, filename: "apsides:/source/<logical-path>" } },
    rolldown: { version: ROLLDOWN_VERSION, options: { authoringFormat: "esm", evaluationFormat: "iife", sourcemap: "hidden" } },
    ses: { version: SES_VERSION, bundle: "dist/ses.umd.min.js", bundleSha256: sha256(sesBytes) },
    compilerFiles, installedTools,
  };
  const compilerClosure: CompilerClosure = {
    schemaVersion: COMPILER_CLOSURE_SCHEMA,
    authoringCompiler: { sdk: sdkIdentity, toolchain },
    nativeCompiler: null,
  };
  const buildInputs = {
    schemaVersion: "apsides.dev/build-inputs/v1",
    entry: entryLogical,
    virtualRoot: "apsides:/source",
    lock: { path: "apsides.lock.json", sha256: sha256(lockBytes), schemaVersion: SOURCE_LOCK_SCHEMA },
    inputs: provenanceInputs,
    sdk: sdkIdentity,
    toolchain,
    compilerClosure,
    compilerClosureDigest: compilerClosureDigest(compilerClosure),
    evaluation: {
      runtime: "deno", entryIdentity: "file:///$deno$stdin.mts", runs: 2, observedEqual: true,
      permissions: { read: "denied", write: "denied", net: "denied", env: "denied", run: "denied", ffi: "denied", sys: "denied", import: "denied" },
      environment: executionEnvironment, randomSeed: 0,
      sandbox: {
        engine: "SES", version: SES_VERSION, lockdown: { errorTaming: "safe", errorTrapping: "none", evalTaming: "safe-eval", localeTaming: "safe", overrideTaming: "severe", regExpTaming: "safe", reporting: "none", stackFiltering: "concise", unhandledRejectionTrapping: "none" },
        endowments: ["buildContext"], hardenedInput: true, moduleLoader: false,
      },
    },
    reproducibility: {
      observedEqual: true, constrainedInputProof: true,
      scope: "Canonical author payload from the digest-locked TypeScript closure evaluated in a pinned SES Compartment with only the hardened declared build input, hosted by the recorded denied-permission Deno runtime.",
      limitation: "This is a bounded source-build contract, not a proof that arbitrary JavaScript is deterministic across unrecorded toolchains or architectures.",
    },
    typecheck: { engine: "deno", status: "passed", remote: false, npm: false, config: { jsx: "react-jsxdev", jsxImportSource: SDK_PACKAGE, importMap: "generated-local-sdk-only" } },
    bundleSha256: sha256(emittedProgram), evaluationBundleSha256: sha256(first.evaluationBundle), evaluationSourceSha256: sha256(evaluateSource), bundleMapSha256: sha256(canonicalString(bundleMap)),
    handlers: [...handlerBundles.files.entries()].map(([id, code]) => ({ id, path: `handlers/${id}.js`, sha256: sha256(code) })).sort((a,b) => order(a.id,b.id)),
  };

  for (const snapshot of author.values()) if (sha256(await readFile(snapshot.physical)) !== snapshot.sha256) throw new Error(`source input changed during build: ${snapshot.logical}`);
  if (sha256(await readFile(lockPath)) !== sha256(lockBytes)) throw new Error("source lock changed during build");
  for (const snapshot of sdk.values()) await assertSnapshotUnchanged(snapshot, "Apsides SDK changed during build");
  if (sha256(await readFile(sesPath)) !== sha256(sesBytes)) throw new Error("SES evaluator changed during build");
  if (sha256(await readFile(deno)) !== sha256(denoBytes)) throw new Error("Deno executable changed during build");
  if (canonicalString(await compilerIdentity()) !== canonicalString(compilerFiles) || canonicalString(await installedToolIdentity()) !== canonicalString(installedTools)) throw new Error("source compiler or installed tool closure changed during build");

  await mkdir(destination);
  if (handlerBundles.files.size > 0) {
    await mkdir(join(destination, "handlers"));
    await Promise.all([...handlerBundles.files.entries()].map(([id, code]) => writeFile(join(destination, "handlers", `${id}.js`), code)));
  }
  await Promise.all([
    writeJson(join(destination, "ir.json"), ir),
    writeFile(join(destination, "program.mjs"), emittedProgram),
    writeJson(join(destination, "program.mjs.map"), bundleMap),
    writeFile(join(destination, "evaluate.mjs"), evaluateSource),
    writeJson(join(destination, "source-map.json"), declarationMap),
    writeJson(join(destination, "build-inputs.json"), buildInputs),
    writeJson(join(destination, "fixtures.json"), fixture.fixtures),
    writeJson(join(destination, "fixture-ir.json"), fixture.fixtureIr),
    writeJson(join(destination, "fixture-overlay.json"), fixture.overlay),
    writeJson(join(destination, "fixture-catalogue.json"), fixture.catalogue),
    writeJson(join(destination, "fixture-coverage.json"), fixture.coverage),
  ]);
};

const bundleSnapshot = async (author: Map<string, Snapshot>, sdk: Map<string, Snapshot>, entry: string): Promise<{bundle: string; evaluationBundle: string; map: unknown; loaded: Set<string>}> => {
  const loaded = new Set<string>();
  const all = new Map<string, Snapshot>([...author.values(), ...sdk.values()].map(value => [value.virtual, value]));
  const authorEntry = author.get(entry)!;
  const wrapper = `import authored from ${JSON.stringify(authorEntry.virtual)};\nimport { renderExport } from ${JSON.stringify(SDK_COMPILER_IMPORT)};\nexport default (buildContext) => renderExport(authored, buildContext);\n`;
  const build = await rolldown({
    input: GENERATED_ENTRY,
    treeshake: false,
    plugins: [{
      name: "apsides-locked-source-closure",
      resolveId(specifier, importer) {
        if (!importer) {
          if (specifier === GENERATED_ENTRY) return specifier;
          throw new Error("undeclared compiler entrypoint");
        }
        if (importer === GENERATED_ENTRY) {
          if (specifier === authorEntry.virtual) return specifier;
          if (specifier === SDK_COMPILER_IMPORT) return sdk.get("compiler.ts")!.virtual;
          throw new Error(`generated compiler entry imports an undeclared module: ${specifier}`);
        }
        if (specifier === SDK_COMPILER_IMPORT) throw new Error("author modules cannot import @apsides/kubernetes/compiler");
        const publicSdk = SDK_PUBLIC_IMPORTS.get(specifier);
        if (publicSdk !== undefined) return sdk.get(publicSdk)!.virtual;
        if (importer.startsWith("apsides:/sdk/")) {
          if (!specifier.startsWith(".")) throw new Error(`SDK import is outside the allowlisted closure: ${specifier}`);
          const importerPath = importer.slice("apsides:/sdk/".length);
          const logical = normalizeLogical(posix.join(posix.dirname(importerPath), specifier), `SDK import ${specifier}`);
          const snapshot = sdk.get(logical);
          if (!snapshot) throw new Error(`SDK import is outside the allowlisted closure: ${logical}`);
          return snapshot.virtual;
        }
        if (!specifier.startsWith(".")) throw new Error(`undeclared package import: ${specifier}`);
        if (!importer.startsWith("apsides:/source/")) throw new Error("SDK cannot import author modules");
        const importerPath = importer.slice("apsides:/source/".length);
        const logical = normalizeLogical(posix.join(posix.dirname(importerPath), specifier), `import ${specifier}`);
        const snapshot = author.get(logical);
        if (!snapshot) throw new Error(`relative import is absent from source lock: ${logical}`);
        return snapshot.virtual;
      },
      load(id) {
        if (id === GENERATED_ENTRY) return wrapper;
        const snapshot = all.get(id);
        if (!snapshot) throw new Error(`undeclared compiler input: ${id}`);
        loaded.add(id);
        const lang = snapshot.logical.endsWith(".tsx") ? "tsx" : snapshot.logical.endsWith(".ts") ? "ts" : "js";
        const compiled = transform(snapshot.virtual, snapshot.text, {
          lang,
          sourcemap: true,
          ...(id.startsWith("apsides:/source/") && lang === "tsx" ? { jsx: { runtime: "automatic" as const, development: true, importSource: SDK_PACKAGE } } : {}),
        });
        if (compiled.errors.length) throw new Error(`Oxc rejected ${snapshot.logical}: ${compiled.errors.map(error => error.message).join("; ")}`);
        if (id.startsWith("apsides:/source/") && lang === "tsx") assertTrustedJsxRuntime(compiled.code, snapshot.logical);
        return compiled.map === undefined ? { code: compiled.code } : { code: compiled.code, map: compiled.map };
      },
      resolveDynamicImport() { throw new Error("runtime imports are not declared build inputs"); },
    }],
  });
  try {
    const result = await build.generate({ format: "esm", sourcemap: "hidden" });
    const chunks = result.output.filter(output => output.type === "chunk");
    if (chunks.length !== 1) throw new Error("build must emit one closed module");
    const evaluation = await build.generate({ format: "iife", name: "ApsidesProgram", exports: "named", sourcemap: false });
    const evaluationChunks = evaluation.output.filter(output => output.type === "chunk");
    if (evaluationChunks.length !== 1) throw new Error("evaluation build must emit one closed script");
    if (!loaded.has(sdk.get("compiler.ts")!.virtual)) throw new Error("source bundle does not use the trusted compiler wrapper");
    return { bundle: chunks[0]!.code, evaluationBundle: evaluationChunks[0]!.code, map: chunks[0]!.map ?? null, loaded };
  } finally { await build.close(); }
};

const assertTrustedJsxRuntime = (code: string, logical: string): void => {
  const body = (parseAst(code, { lang: "js", sourceType: "module" }) as { body?: unknown[] }).body ?? [];
  for (const node of body) {
    if (!node || typeof node !== "object" || (node as { type?: unknown }).type !== "ImportDeclaration") continue;
    const value = ((node as { source?: { value?: unknown } }).source?.value);
    if (typeof value === "string" && value.endsWith("/jsx-dev-runtime") && value !== `${SDK_PACKAGE}/jsx-dev-runtime`) {
      throw new Error(`author TSX cannot override the trusted JSX import source: ${logical}`);
    }
  }
};

const typecheckSnapshot = async (author: Map<string, Snapshot>, sdk: Map<string, Snapshot>, entry: string, deno: string): Promise<void> => {
  const stage = await mkdtemp(join(tmpdir(), "apsides-typecheck-"));
  try {
    const sourceDir = join(stage, "source");
    const sdkDir = join(stage, "sdk");
    await mkdir(sourceDir, { recursive: true }); await mkdir(sdkDir, { recursive: true });
    for (const item of author.values()) { const target = join(sourceDir, item.logical); await mkdir(dirname(target), {recursive:true}); await writeFile(target, item.text); }
    for (const item of sdk.values()) await writeFile(join(sdkDir, item.logical), item.text);
    const denoConfig = join(stage, "deno.json");
    const imports = Object.fromEntries([...SDK_PUBLIC_IMPORTS].map(([specifier, logical]) => [specifier, pathToFileURL(join(sdkDir, logical)).href]));
    await writeFile(denoConfig, JSON.stringify({ compilerOptions: { jsx: "react-jsxdev", jsxImportSource: SDK_PACKAGE, strict: true }, imports }));
    await run(deno, ["check", "--config", denoConfig, "--no-lock", "--no-npm", "--no-remote", "--deny-import", join(sourceDir, entry)], "", { ...executionEnvironment });
  } finally { await rm(stage, {recursive:true, force:true}); }
};

const evaluator = (ses: string, bundle: string, buildContext: unknown): string => {
  const guest = `${bundle}\n; (async () => {\n  return {productionIR: ApsidesProgram.default(buildContext)};\n})()`;
  return `const emit = console.log.bind(console);
// Deno exposes a Node-compatible process.env proxy that throws under --deny-env.
// SES receives no environment options; all lockdown choices below are explicit.
Object.defineProperty(globalThis, "process", { value: undefined, configurable: false, writable: false });
${ses}
lockdown({errorTaming:"safe",errorTrapping:"none",evalTaming:"safe-eval",localeTaming:"safe",overrideTaming:"severe",regExpTaming:"safe",reporting:"none",stackFiltering:"concise",unhandledRejectionTrapping:"none"});
const compartment = new Compartment({buildContext:harden(${JSON.stringify(buildContext)})});
const payload = await compartment.evaluate(${JSON.stringify(guest)});
emit(JSON.stringify(payload));
`;
};

const denoEvaluationArgs = (): string[] => {
  return ["run", "--no-config", "--no-lock", "--no-npm", "--cached-only", "--no-code-cache", "--no-prompt", "--deny-read", "--deny-write", "--deny-net", "--deny-env", "--deny-run", "--deny-ffi", "--deny-sys", "--deny-import", "--seed=0", "--v8-flags=--max-old-space-size=128", "-"];
};

const parseEvaluation = (stdout: string): { productionIR: ProductionIr } => {
  const line = stdout.trim();
  if (!line || line.includes("\n")) throw new Error("source evaluation must emit exactly one JSON payload");
  const value = JSON.parse(line) as Record<string, unknown>;
  if (!value || typeof value !== "object" || Array.isArray(value) || !("productionIR" in value)) throw new Error("source evaluation emitted an invalid payload");
  return { productionIR: value.productionIR as ProductionIr };
};

/**
 * Bundle every declared handler as its own deterministic, closed ESM entry
 * (no externals: only other locked source modules may be imported, never an
 * npm or remote specifier, never the authoring SDK), and fill in its
 * `bundleSha256`. Each handler is bundled twice and compared byte-for-byte,
 * matching the determinism guarantee the top-level program bundle already
 * gets from `first`/`second` in {@link compileApsides}.
 */
const bundleHandlers = async (author: Map<string, Snapshot>, ir: ProductionIr): Promise<{ handlers: ProductionIr["handlers"]; files: Map<string, string>; loaded: Set<string> }> => {
  const files = new Map<string, string>();
  const handlers: ProductionIr["handlers"] = [];
  const loaded = new Set<string>();
  for (const handler of ir.handlers) {
    if (!author.has(handler.module)) throw new Error(`handler module is absent from the source lock (resolved from the lock root, apsides.lock.json's own directory): ${handler.module}`);
    const first = await bundleHandlerModule(author, handler.module);
    const second = await bundleHandlerModule(author, handler.module);
    if (first.code !== second.code) throw new Error(`independent handler bundles for ${handler.id} were not byte-identical`);
    if (Buffer.byteLength(first.code) > MAX_OUTPUT_BYTES) throw new Error(`handler ${handler.id} bundle exceeds its output budget`);
    files.set(handler.id, first.code);
    for (const id of first.loaded) loaded.add(id);
    handlers.push({ ...handler, bundleSha256: sha256(first.code) });
  }
  return { handlers, files, loaded };
};

const bundleHandlerModule = async (author: Map<string, Snapshot>, entry: string): Promise<{ code: string; loaded: Set<string> }> => {
  const entrySnapshot = author.get(entry)!;
  const loaded = new Set<string>();
  const build = await rolldown({
    input: entrySnapshot.virtual,
    treeshake: false,
    plugins: [{
      name: "apsides-locked-handler-closure",
      resolveId(specifier, importer) {
        if (!importer) {
          if (specifier === entrySnapshot.virtual) return specifier;
          throw new Error("undeclared handler entrypoint");
        }
        if (!specifier.startsWith(".")) throw new Error(`handler module cannot import an undeclared package: ${specifier}`);
        if (!importer.startsWith("apsides:/source/")) throw new Error("handler bundling is restricted to locked source modules");
        const importerPath = importer.slice("apsides:/source/".length);
        const logical = normalizeLogical(posix.join(posix.dirname(importerPath), specifier), `handler import ${specifier}`);
        const snapshot = author.get(logical);
        if (!snapshot) throw new Error(`handler import is absent from the source lock: ${logical}`);
        return snapshot.virtual;
      },
      load(id) {
        const snapshot = [...author.values()].find(value => value.virtual === id);
        if (!snapshot) throw new Error(`undeclared handler bundle input: ${id}`);
        loaded.add(id);
        const lang = snapshot.logical.endsWith(".tsx") ? "tsx" : snapshot.logical.endsWith(".ts") ? "ts" : "js";
        const compiled = transform(snapshot.virtual, snapshot.text, { lang, sourcemap: false });
        if (compiled.errors.length) throw new Error(`Oxc rejected ${snapshot.logical}: ${compiled.errors.map(error => error.message).join("; ")}`);
        return { code: compiled.code };
      },
      resolveDynamicImport() { throw new Error("handler dynamic imports are not declared build inputs"); },
    }],
  });
  try {
    const result = await build.generate({ format: "esm", sourcemap: false });
    const chunks = result.output.filter(output => output.type === "chunk");
    if (chunks.length !== 1) throw new Error("handler build must emit one closed module");
    return { code: chunks[0]!.code, loaded };
  } finally { await build.close(); }
};

const validateSourceLocations = (ir: ProductionIr, author: Map<string, Snapshot>): void => {
  for (const declaration of [...ir.resources, ...ir.inputs, ...ir.reactions, ...ir.handlers]) {
    const source = declaration.source;
    const file = author.get(source?.file);
    if (!file) throw new Error(`declaration ${declaration.id} source is not in the locked source closure: ${source?.file}`);
    const lines = file.text.split(/\r?\n/);
    if (!Number.isSafeInteger(source.line) || source.line < 1 || source.line > lines.length || !Number.isSafeInteger(source.column) || source.column < 1 || source.column > (lines[source.line - 1]?.length ?? 0) + 1) throw new Error(`declaration ${declaration.id} has an invalid source location`);
  }
};

const buildDeclarationMap = (ir: ProductionIr, fixtures: {inputs:{inputId:string;source:{file:string;line:number;column:number}}[];workloadSubstitutions:{targetResourceId:string;source:{file:string;line:number;column:number}}[]}, author: Map<string, Snapshot>) => {
  const entry = (category: string, id: string, source: {file:string;line:number;column:number}) => ({ category, id, source, inputSha256: author.get(source.file)!.sha256 });
  return {
    schemaVersion: "apsides.dev/source-map/v1", virtualRoot: "apsides:/source",
    declarations: [
      ...ir.resources.map(value => entry("resource", value.id, value.source)), ...ir.inputs.map(value => entry("input", value.id, value.source)),
      ...ir.reactions.map(value => entry("reaction", value.id, value.source)), ...ir.handlers.map(value => entry("handler", value.id, value.source)),
      ...fixtures.inputs.map(value => entry("fixture-input", value.inputId, value.source)),
      ...fixtures.workloadSubstitutions.map(value => entry("fixture-workload", value.targetResourceId, value.source)),
    ].sort((a,b) => order(`${a.category}/${a.id}`, `${b.category}/${b.id}`)),
  };
};

const normalizeSourceMap = (value: unknown): unknown => {
  if (!value || typeof value !== "object") throw new Error("Rolldown did not emit a source map");
  const map = JSON.parse(String(value)) as Record<string, unknown>;
  if (Array.isArray(map.sources)) map.sources = map.sources.map(source => {
    const text = String(source);
    const offset = text.lastIndexOf("apsides:/");
    return offset < 0 ? text : text.slice(offset);
  });
  delete map.file;
  return map;
};

const parseCanonicalLock = (text: string): SourceLock => {
  const value = JSON.parse(text) as Record<string, unknown>;
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("source lock must be an object");
  if (canonicalPretty(value) !== text) throw new Error("source lock must be canonical JSON; duplicate, unordered, or unknown formatting is unsupported");
  if (value.schemaVersion !== SOURCE_LOCK_SCHEMA || typeof value.entry !== "string" || !Array.isArray(value.inputs)) throw new Error("unsupported source lock");
  if (Object.keys(value).sort(order).join(",") !== "entry,inputs,schemaVersion") throw new Error("source lock contains unknown fields");
  const seen = new Set<string>();
  const inputs = value.inputs.map((raw, index) => {
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error(`source lock input ${index} must be an object`);
    const item = raw as Record<string, unknown>;
    if (Object.keys(item).sort(order).join(",") !== "path,sha256" || typeof item.path !== "string" || typeof item.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(item.sha256)) throw new Error(`invalid source lock input ${index}`);
    const path = normalizeLogical(item.path, `source lock input ${index}`);
    if (!/\.(ts|tsx|js|mjs)$/.test(path)) throw new Error(`unsupported source module extension: ${path}`);
    if (seen.has(path)) throw new Error(`duplicate source lock input: ${path}`); seen.add(path);
    return { path, sha256: item.sha256 };
  });
  if (inputs.length === 0 || inputs.length > MAX_MODULES) throw new Error(`source lock must contain 1..${MAX_MODULES} inputs`);
  const sorted = [...inputs].sort((a,b) => order(a.path,b.path));
  if (canonicalString(inputs) !== canonicalString(sorted)) throw new Error("source lock inputs must be sorted by path");
  return { schemaVersion: SOURCE_LOCK_SCHEMA, entry: normalizeLogical(value.entry, "source lock entry"), inputs };
};

const normalizeLogical = (value: string, context: string): string => {
  if (!value || value.startsWith("/") || value.includes("\\") || value.includes("?") || value.includes("#")) throw new Error(`${context} must be a normalized relative path`);
  const normalized = posix.normalize(value);
  if (normalized !== value || normalized === ".." || normalized.startsWith("../") || normalized.split("/").includes(".")) throw new Error(`${context} must be a normalized relative path`);
  return normalized;
};

const snapshotSdk = async (): Promise<Map<string, Snapshot>> => {
  const root = await realpath(fileURLToPath(new URL("../../kubernetes/src", import.meta.url)));
  const snapshots = new Map<string, Snapshot>();
  for (const logical of SDK_FILES) {
    const physical = join(root, logical);
    const actual = await realpath(physical);
    if (actual !== physical || !(await lstat(physical)).isFile()) throw new Error(`Apsides SDK module must be a regular non-symlink file: ${logical}`);
    const bytes = await readFile(physical);
    snapshots.set(logical, { logical, virtual: `apsides:/sdk/${logical}`, physical, text: bytes.toString("utf8"), sha256: sha256(bytes) });
  }
  return snapshots;
};

const assertSnapshotUnchanged = async (snapshot: Snapshot, message: string): Promise<void> => {
  let actual: string;
  try { actual = await realpath(snapshot.physical); } catch { throw new Error(`${message}: ${snapshot.logical}`); }
  if (actual !== snapshot.physical || !(await lstat(snapshot.physical)).isFile() || sha256(await readFile(snapshot.physical)) !== snapshot.sha256) {
    throw new Error(`${message}: ${snapshot.logical}`);
  }
};

const compilerIdentity = async (): Promise<{path:string;sha256:string}[]> => {
  const base = dirname(fileURLToPath(import.meta.url));
  const paths = [fileURLToPath(import.meta.url), fileURLToPath(new URL("./fixtures.ts", import.meta.url))];
  return Promise.all(paths.map(async path => ({ path: `@apsides/compiler/${relative(base, path)}`, sha256: sha256(await readFile(path)) })));
};

const installedToolIdentity = async (): Promise<{path:string;sha256:string}[]> => {
  const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const workspaceRoot = resolve(packageRoot, "../..");
  const oxcRoot = await findPackageRoot(fileURLToPath(import.meta.resolve("oxc-transform")));
  const rolldownRoot = await findPackageRoot(fileURLToPath(import.meta.resolve("rolldown")));
  const sesRoot = await findPackageRoot(fileURLToPath(import.meta.resolve("ses")));
  const binding = nativeBindingTarget();
  const oxcRequire = createRequire(join(oxcRoot,"package.json"));
  const rolldownRequire = createRequire(join(rolldownRoot,"package.json"));
  const roots = [
    ["packages/compiler", packageRoot],
    ["node_modules/oxc-transform", oxcRoot],
    ["node_modules/rolldown", rolldownRoot],
    ["node_modules/ses", sesRoot],
    [`node_modules/@oxc-transform/binding-${binding}`, await findPackageRoot(oxcRequire.resolve(`@oxc-transform/binding-${binding}`))],
    [`node_modules/@rolldown/binding-${binding}`, await findPackageRoot(rolldownRequire.resolve(`@rolldown/binding-${binding}`))],
    ["node_modules/@rolldown/pluginutils", await findPackageRoot(rolldownRequire.resolve("@rolldown/pluginutils"))],
    ["node_modules/ansis", await findPackageRoot(rolldownRequire.resolve("ansis"))],
  ] as const;
  const output: {path:string;sha256:string}[] = await Promise.all(roots.map(async ([path, root]) => ({ path, sha256: await directoryDigest(root, path, path === "packages/compiler" ? new Set(["src", "node_modules"]) : new Set()) })));
  output.push({path:"deno.lock",sha256:sha256(await readFile(join(workspaceRoot,"deno.lock")))});
  return output.sort((a,b) => order(a.path,b.path));
};

const nativeBindingTarget = (): string => {
  const target = Deno.build.target;
  if (target.endsWith("-apple-darwin")) return `darwin-${process.arch}`;
  if (target.endsWith("-pc-windows-msvc")) return `win32-${process.arch}-msvc`;
  if (target.endsWith("-unknown-linux-gnu")) return `linux-${process.arch}-gnu`;
  if (target.endsWith("-unknown-linux-musl")) return `linux-${process.arch}-musl`;
  throw new Error(`unsupported source compiler target ${target}`);
};

const findPackageRoot = async (path: string): Promise<string> => {
  let current = dirname(path);
  while (true) {
    if (await exists(join(current,"package.json"))) return realpath(current);
    const parent = dirname(current);
    if (parent === current) throw new Error(`cannot find package root for ${path}`);
    current = parent;
  }
};

const directoryDigest = async (root: string, logicalRoot: string, excluded: Set<string>): Promise<string> => {
  const entries: {path:string;sha256:string}[] = [];
  const visit = async (directory: string): Promise<void> => {
    for (const entry of (await readdir(directory,{withFileTypes:true})).sort((a,b) => order(a.name,b.name))) {
      const path = join(directory,entry.name);
      const logical = relative(root,path).split(sep).join("/");
      if (excluded.has(logical.split("/")[0]!)) continue;
      if (entry.isSymbolicLink()) throw new Error(`installed tool closure contains an undeclared symlink: ${logicalRoot}/${logical}`);
      if (entry.isDirectory()) await visit(path);
      else if (entry.isFile()) entries.push({path:`${logicalRoot}/${logical}`,sha256:sha256(await readFile(path))});
      else throw new Error(`installed tool closure contains an unsupported file type: ${logicalRoot}/${logical}`);
    }
  };
  await visit(root);
  return sha256(canonicalString(entries));
};

const run = (command: string, args: string[], stdin: string, env: Record<string,string>): Promise<{stdout:string;stderr:string}> => {
  return new Promise((resolveRun, rejectRun) => {
    const child = spawn(command, args, { cwd: "/", env, stdio: ["pipe", "pipe", "pipe"] });
    const stdout: Buffer[] = []; const stderr: Buffer[] = []; let size = 0;
    const timer = setTimeout(() => child.kill("SIGKILL"), 30_000);
    child.stdout.on("data", (chunk: Buffer) => { size += chunk.length; if (size > MAX_OUTPUT_BYTES) child.kill("SIGKILL"); else stdout.push(chunk); });
    child.stderr.on("data", (chunk: Buffer) => { size += chunk.length; if (size > MAX_OUTPUT_BYTES) child.kill("SIGKILL"); else stderr.push(chunk); });
    child.on("error", error => { clearTimeout(timer); rejectRun(error); });
    child.on("close", code => {
      clearTimeout(timer);
      const output = {stdout:Buffer.concat(stdout).toString("utf8"),stderr:Buffer.concat(stderr).toString("utf8")};
      if (code !== 0) rejectRun(new Error(`${command} failed (${String(code)}): ${output.stderr.trim() || output.stdout.trim()}`)); else resolveRun(output);
    });
    child.stdin.on("error", error => {
      clearTimeout(timer);
      child.kill("SIGKILL");
      rejectRun(error);
    });
    child.stdin.end(stdin);
  });
};

const writeJson = async (path: string, value: unknown): Promise<void> => { await writeFile(path, `${JSON.stringify(JSON.parse(canonicalString(value)), null, 2)}\n`); };
const canonicalPretty = (value: unknown): string => { return `${JSON.stringify(JSON.parse(canonicalString(value)), null, 2)}\n`; };
const exists = async (path: string): Promise<boolean> => { try { await lstat(path); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return false; throw error; } };

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [source, output, ...extra] = process.argv.slice(2);
  if (!source || !output || extra.length) {
    console.error("usage: compiler.ts SOURCE OUTPUT");
    process.exitCode = 2;
  } else {
    try {
      await compileApsides(resolve(source), resolve(output));
    } catch {
      // Author code and tool diagnostics may contain private source or values.
      console.error("apsides: source compilation failed");
      process.exitCode = 1;
    }
  }
}
