import { createHash } from "node:crypto";
import { access, lstat, mkdtemp, open, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { tmpdir } from "node:os";
import { transform } from "oxc-transform";
import { rolldown } from "rolldown";
import { parseAst } from "rolldown/parseAst";

import { encodeBundleArchive } from "./archive.js";

export { encodeBundleArchive } from "./archive.js";
export type { BundleArchiveEntries, BundleArchiveEntryName } from "./archive.js";

export interface BundleOptions {
  readonly rootDir: string;
  readonly compilerVersion?: string;
  readonly bundlerVersion?: string;
}
export interface FileIntegrity { readonly path: string; readonly length: number; readonly sha256: string; }
export interface BundleIntegrityManifestV0 { readonly manifestVersion: string; readonly compiler: { identity: string; version: string }; readonly bundler: { identity: string; version: string }; readonly rendererAbiVersion: string; readonly descriptor: FileIntegrity; readonly renderModules: Array<FileIntegrity & { identity: string }>; readonly actionModules: Array<FileIntegrity & { identity: string }>; readonly sourceMaps: FileIntegrity[]; readonly phaseModules: Array<{ phase: string; moduleIdentity: string }>; }
export interface BuiltBundle { readonly bundleDigest: string; readonly manifest: BundleIntegrityManifestV0; readonly archiveBytes: Buffer; }

const ABI = "renderer.oxideloop.dev/v0";
const RENDER_IDENTITY = "demo.web-application.render";
const ACTION_IDENTITY = "demo.web-application.input-changed";

/**
 * Build in a private staging directory and return one archive snapshot. Local
 * execution trusts this process and its private staging; the untrusted
 * boundary is the archive bytes later consumed by Rust.
 */
export async function buildBundle(options: BundleOptions): Promise<BuiltBundle> {
  const rootDir = resolve(options.rootDir);
  const workspaceRoot = await findWorkspaceRoot(rootDir);
  await validateAuthoredSources(rootDir);
  let staging: string | undefined;
  try {
    staging = await mkdtemp(join(tmpdir(), "oxideloop-bundle-"));
    const actionPath = join(staging, "action.callable.js");
    await bundle(join(rootDir, "action.ts"), actionPath, "OxideLoopAction", "action", workspaceRoot, rootDir);
    const actionFile = await integrity(actionPath, staging);

    // The action artifact is complete before its raw digest is compiled into
    // render. The final bundle digest is *not* compiled into render bytes:
    // the renderer receives it from the canonical RenderInputV0 at invocation.
    const renderPath = join(staging, "render.callable.js");
    await bundle(join(rootDir, "render.tsx"), renderPath, "OxideLoopRender", "render", workspaceRoot, rootDir, {
      __OXIDELOOP_ACTION_MODULE_DIGEST__: JSON.stringify(actionFile.sha256),
    });

    const descriptorPath = join(staging, "descriptor.json");
    const descriptor = {
      descriptorVersion: "oxideloop.dev/descriptor/v0",
      rendererAbiVersion: ABI,
      renderModule: { identity: RENDER_IDENTITY, digest: "" },
      actionModules: [{ identity: ACTION_IDENTITY, digest: actionFile.sha256 }],
    };
    const renderFile = await integrity(renderPath, staging);
    descriptor.renderModule.digest = renderFile.sha256;
    await writeFile(descriptorPath, `${JSON.stringify(descriptor, null, 2)}\n`, "utf8");

    const manifest: BundleIntegrityManifestV0 = {
      manifestVersion: "bundle.oxideloop.dev/v0",
      compiler: { identity: "oxc", version: options.compilerVersion ?? "0.58.0" },
      bundler: { identity: "rolldown", version: options.bundlerVersion ?? "1.0.0-beta.35" },
      rendererAbiVersion: ABI,
      descriptor: await integrity(descriptorPath, staging),
      renderModules: [{ identity: RENDER_IDENTITY, ...renderFile }],
      actionModules: [{ identity: ACTION_IDENTITY, ...actionFile }],
      sourceMaps: [],
      phaseModules: [
        { phase: "render", moduleIdentity: RENDER_IDENTITY },
        { phase: "action", moduleIdentity: ACTION_IDENTITY },
      ],
    };
    const manifestBytes = canonical(manifest);
    const bundleDigest = hash(manifestBytes);
    const installBytes = canonical({ bundleDigest, manifest });
    const archiveBytes = encodeBundleArchive({
      "action.callable.js": await readOwnedStagingFile(actionPath),
      "bundle-install.json": Buffer.from(installBytes, "utf8"),
      "descriptor.json": await readOwnedStagingFile(descriptorPath),
      "integrity-manifest.json": Buffer.from(manifestBytes, "utf8"),
      "render.callable.js": await readOwnedStagingFile(renderPath),
    });
    return { bundleDigest, manifest, archiveBytes: Buffer.from(archiveBytes) };
  } finally {
    if (staging) await rm(staging, { recursive: true, force: true });
  }
}

async function validateAuthoredSources(rootDir: string): Promise<void> {
  const realRoot = await canonicalPath(rootDir);
  const lexicalRoot = resolve(rootDir);
  for (const entry of [join(rootDir, "action.ts"), join(rootDir, "render.tsx")]) {
    await assertNoSymlinkBelow(lexicalRoot, resolve(entry));
    const resolved = await canonicalPath(entry);
    if (!isContainedPath(realRoot, resolved) || resolved === realRoot) throw new Error("authored entrypoint escapes rootDir");
  }
}


async function bundle(
  input: string,
  output: string,
  name: string,
  exportName: "render" | "action",
  workspaceRoot: string,
  authoredRoot: string,
  define: Record<string, string> = {},
): Promise<void> {
  const source = await readFile(input, "utf8");
  const transformed = transform(input, source, {
    lang: input.endsWith(".tsx") ? "tsx" : "ts",
    jsx: { runtime: "automatic", importSource: "@oxideloop/react-like-runtime" },
  });
  const workingDir = dirname(output);
  const intermediate = join(workingDir, `${basename(input)}.compiled.mjs`);
  await writeFile(intermediate, transformed.code, "utf8");
  const transformedSources = new Map<string, string>();
  const aliases = {
    "@oxideloop/sdk": join(workspaceRoot, "packages/sdk/src/index.ts"),
    "@oxideloop/sdk/hooks": join(workspaceRoot, "packages/sdk/src/hooks.ts"),
    "@oxideloop/react-like-runtime": join(workspaceRoot, "packages/react-like-runtime/src/index.ts"),
    "@oxideloop/react-like-runtime/jsx-runtime": join(workspaceRoot, "packages/react-like-runtime/src/jsx-runtime.ts"),
  } as const;
  const canonicalAliases = await canonicalizeAliases(aliases);
  const runtimeRoots = await Promise.all([
    canonicalPath(join(workspaceRoot, "packages/sdk/src")),
    canonicalPath(join(workspaceRoot, "packages/react-like-runtime/src")),
  ]);
  const build = await rolldown({
    input: intermediate,
    define: { "process.env.NODE_ENV": JSON.stringify("production"), ...define },
    plugins: [{
      name: "oxideloop-workspace-aliases",
      async resolveId(source: string, importer?: string) {
        // Rolldown may hand plugins virtual ids. Always establish the importer
        // identity before considering an alias, so a foreign module can never
        // borrow the authored graph's trust.
        if (!importer) {
          // The sole importer-less resolution is Rolldown's declared staged
          // entrypoint. Every other graph edge must carry a classified parent.
          if (await sameCanonicalPath(source, intermediate)) return intermediate;
          throw new Error(`unapproved import without importer: ${source}`);
        }
        const state = await classifyImporter(importer, authoredRoot, intermediate, runtimeRoots);
        const alias = canonicalAliases[source as keyof typeof canonicalAliases];
        if (alias) {
          if (state === "unknown") throw new Error(`unapproved alias import from unknown importer: ${source}`);
          return alias;
        }
        if (source.startsWith("/") || isAbsolute(source)) {
          throw new Error(`absolute imports are not allowed: ${source}`);
        }
        if (!source.startsWith(".")) {
          throw new Error(`unapproved bare import from ${state} module: ${source}`);
        }
        if (state === "unknown") throw new Error(`unapproved relative import from unknown importer: ${source}`);
        if (state === "entrypoint") return resolveAuthoredModule(authoredRoot, dirname(input), source);
        if (state === "authored") return resolveAuthoredModule(authoredRoot, dirname(await canonicalPath(importer)), source);
        return resolveTrustedRuntimeModule(runtimeRoots, dirname(await canonicalPath(importer)), source);
      },
      async load(id: string) {
        const root = await canonicalPath(authoredRoot);
        let canonical: string;
        try { canonical = await canonicalPath(id); } catch { return null; }
        const runtime = runtimeRoots.some((runtimeRoot) => isContainedPath(runtimeRoot, canonical));
        if ((!isContainedPath(root, canonical) && !runtime) || !/\.(?:ts|tsx)$/.test(canonical)) return null;
        const cached = transformedSources.get(canonical);
        if (cached !== undefined) return cached;
        const source = await readFile(canonical, "utf8");
        const compiled = transform(canonical, source, {
          lang: canonical.endsWith(".tsx") ? "tsx" : "ts",
          jsx: { runtime: "automatic", importSource: "@oxideloop/react-like-runtime" },
        }).code;
        transformedSources.set(canonical, compiled);
        return compiled;
      },
      transform(code: string) {
        // Rolldown's resolveDynamicImport hook is not invoked for every
        // expression-valued import in this pinned release. Parse each module
        // with Rolldown's pinned Oxc parser so literal and nonliteral AST
        // ImportExpression nodes fail before code generation. This deliberately
        // does not confuse strings, comments, templates, or regular expressions
        // containing the text `import(` with executable syntax.
        if (containsImportExpression(parseAst(code, { lang: "js", sourceType: "module" }))) {
          throw new Error("dynamic imports are not allowed in bundle modules");
        }
        return null;
      },
      async resolveDynamicImport(_specifier: string, importer?: string) {
        // A dynamic import is never part of this static, single-file ABI.
        // This applies to literal and expression specifiers alike.
        if (importer) throw new Error("dynamic imports are not allowed in bundle modules");
        throw new Error("dynamic imports are not allowed without an importer");
      },
    }],
  });
  try {
    const generated = await build.generate({ format: "iife", name, minify: true });
    const chunks = generated.output.filter((item) => item.type === "chunk");
    if (chunks.length !== 1) throw new Error(`expected one chunk for ${input}`);
    const code = (chunks[0] as { code: string }).code;
    const adapter = exportName === "render"
      ? `return ${name}.render(input);`
      : `return ${name}.action({eventMetadata:input.eventMetadata,occurrenceId:input.occurrenceId,canonical:input});`;
    await writeFile(output, `((input)=>{\n${code}\n${adapter}\n})\n`, "utf8");
  } finally {
    await build.close();
    await rm(intermediate, { force: true });
  }
}

function containsImportExpression(node: unknown, seen = new WeakSet<object>()): boolean {
  if (node === null || typeof node !== "object") return false;
  if (seen.has(node)) return false;
  seen.add(node);
  if ((node as { type?: unknown }).type === "ImportExpression") return true;
  if (Array.isArray(node)) return node.some((value) => containsImportExpression(value, seen));
  return Object.values(node).some((value) => containsImportExpression(value, seen));
}

async function canonicalizeAliases<T extends Record<string, string>>(aliases: T): Promise<{ readonly [K in keyof T]: string }> {
  const entries = await Promise.all(Object.entries(aliases).map(async ([name, path]) => [name, await canonicalPath(path)] as const));
  return Object.fromEntries(entries) as { readonly [K in keyof T]: string };
}

async function resolveAuthoredModule(root: string, from: string, source: string): Promise<string> {
  return resolveStaticTypeScriptModule(root, from, source, "authored");
}

async function resolveTrustedRuntimeModule(roots: readonly string[], from: string, source: string): Promise<string> {
  const owner = roots.find((root) => isContainedPath(root, from));
  if (!owner) throw new Error(`trusted runtime importer escapes fixed roots: ${from}`);
  return resolveStaticTypeScriptModule(owner, from, source, "trusted runtime");
}

async function resolveStaticTypeScriptModule(root: string, from: string, source: string, label: string): Promise<string> {
  const realRoot = await canonicalPath(root);
  const lexicalRoot = resolve(root);
  if (source.startsWith("/") || !source.startsWith(".")) throw new Error(`unapproved authored import: ${source}`);
  if (/\.(?:mjs|cjs|jsx|json|css|tsbuildinfo)$/.test(source)) {
    throw new Error(`unsupported authored import: ${source}`);
  }
  const raw = resolve(from, source);
  const candidates = /\.js$/.test(raw)
    ? [raw.slice(0, -3) + ".ts", raw.slice(0, -3) + ".tsx"]
    : /\.[^/]+$/.test(raw)
      ? [raw]
      : [raw + ".ts", raw + ".tsx", join(raw, "index.ts"), join(raw, "index.tsx")];
  for (const candidate of candidates) {
    try {
      await assertNoSymlinkBelow(lexicalRoot, candidate);
      const resolved = await canonicalPath(candidate);
      if (!isContainedPath(realRoot, resolved) || !/\.(?:ts|tsx)$/.test(resolved)) throw new Error(`${label} import escapes fixed root: ${source}`);
      return resolved;
    } catch (error) {
      if (error instanceof Error && (error.message.includes("escapes rootDir") || error.message.includes("escapes fixed root"))) throw error;
    }
  }
  throw new Error(`cannot resolve authored import ${source}`);
}

async function assertNoSymlinkBelow(root: string, target: string): Promise<void> {
  const relativePath = relative(root, resolve(target));
  if (!isContainedPath(root, resolve(target)) || relativePath === "") throw new Error("authored import escapes rootDir");
  let cursor = root;
  for (const component of relativePath.split(/[/\\]/)) {
    cursor = join(cursor, component);
    try { if ((await lstat(cursor)).isSymbolicLink()) throw new Error("authored import escapes rootDir"); }
    catch (error) { if (error instanceof Error && error.message.includes("escapes rootDir")) throw error; return; }
  }
}

async function canonicalPath(path: string): Promise<string> {
  return realpath(resolve(path));
}

async function sameCanonicalPath(left: string, right: string): Promise<boolean> {
  try { return (await canonicalPath(left)) === (await canonicalPath(right)); }
  catch { return false; }
}

function isContainedPath(root: string, target: string): boolean {
  const rootResolved = resolve(root);
  const targetResolved = resolve(target);
  if (isAbsolute(relative(rootResolved, targetResolved)) || isAbsolute(relative(targetResolved, rootResolved))) return false;
  const rel = relative(rootResolved, targetResolved);
  return rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !rel.startsWith("../") && !rel.startsWith("..\\"));
}

type ImporterState = "entrypoint" | "authored" | "runtime" | "unknown";

async function classifyImporter(importer: string, root: string, intermediate: string, runtimeRoots: readonly string[]): Promise<ImporterState> {
  let canonical: string;
  try { canonical = await canonicalPath(importer); } catch { return "unknown"; }
  const realRoot = await canonicalPath(root);
  const realIntermediate = await canonicalPath(intermediate);
  if (canonical === realIntermediate) return "entrypoint";
  if (isContainedPath(realRoot, canonical) && /\.(?:ts|tsx)$/.test(canonical)) return "authored";
  if (runtimeRoots.some((runtimeRoot) => isContainedPath(runtimeRoot, canonical)) && /\.(?:ts|tsx)$/.test(canonical)) return "runtime";
  return "unknown";
}

async function readOwnedStagingFile(path: string): Promise<Buffer> {
  const handle = await open(path, "r");
  try {
    const before = await handle.stat();
    if (!before.isFile() || before.size > 128 * 1024) throw new Error(`generated archive source is invalid: ${path}`);
    const bytes = await handle.readFile();
    const after = await handle.stat();
    if (!after.isFile() || before.size !== after.size || bytes.length !== before.size) {
      throw new Error(`generated archive source changed while reading: ${path}`);
    }
    return bytes;
  } finally {
    await handle.close();
  }
}

async function findWorkspaceRoot(from: string): Promise<string> {
  let candidate = from;
  while (dirname(candidate) !== candidate) {
    try {
      await access(join(candidate, "pnpm-workspace.yaml"));
      return candidate;
    } catch { candidate = dirname(candidate); }
  }
  throw new Error(`could not find pnpm workspace root above ${from}`);
}

async function integrity(path: string, root: string): Promise<FileIntegrity> {
  const bytes = await readFile(path);
  return { path: relative(root, path).replaceAll("\\", "/"), length: bytes.byteLength, sha256: hash(bytes) };
}
function hash(value: string | Uint8Array): string { return createHash("sha256").update(value).digest("hex"); }
/** Mirrors the renderer ABI's canonical JSON object ordering (UTF-16 units). */
function canonical(value: unknown): string {
  if (value === null || typeof value === "boolean" || typeof value === "number" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (typeof value !== "object") throw new Error("bundle metadata must be JSON data");
  const object = value as Record<string, unknown>;
  return `{${Object.keys(object).sort(compareUtf16).map((key) => `${JSON.stringify(key)}:${canonical(object[key])}`).join(",")}}`;
}

function compareUtf16(left: string, right: string): number {
  for (let index = 0; index < Math.min(left.length, right.length); index += 1) {
    const difference = left.charCodeAt(index) - right.charCodeAt(index);
    if (difference !== 0) return difference;
  }
  return left.length - right.length;
}
