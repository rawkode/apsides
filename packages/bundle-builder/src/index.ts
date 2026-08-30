import { createHash } from "node:crypto";
import { access, lstat, mkdtemp, open, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { basename, dirname, join, relative, resolve } from "node:path";
import { tmpdir } from "node:os";
import { transform } from "oxc-transform";
import { rolldown } from "rolldown";

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
  const realRoot = await realpath(rootDir);
  for (const entry of [join(rootDir, "action.ts"), join(rootDir, "render.tsx")]) {
    const info = await lstat(entry);
    if (info.isSymbolicLink()) throw new Error(`authored entrypoint is a symlink: ${entry}`);
    const resolved = await realpath(entry);
    if (!resolved.startsWith(`${realRoot}/`)) throw new Error("authored entrypoint escapes rootDir");
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
  const aliases = {
    "@oxideloop/sdk": join(workspaceRoot, "packages/sdk/src/index.ts"),
    "@oxideloop/sdk/hooks": join(workspaceRoot, "packages/sdk/src/hooks.ts"),
    "@oxideloop/react-like-runtime": join(workspaceRoot, "packages/react-like-runtime/src/index.ts"),
    "@oxideloop/react-like-runtime/jsx-runtime": join(workspaceRoot, "packages/react-like-runtime/src/jsx-runtime.ts"),
  } as const;
  const build = await rolldown({
    input: intermediate,
    define: { "process.env.NODE_ENV": JSON.stringify("production"), ...define },
    plugins: [{
      name: "oxideloop-workspace-aliases",
      async resolveId(source: string, importer?: string) {
        const alias = aliases[source as keyof typeof aliases];
        if (alias) return alias;
        if (!importer) return null;
        const resolvedImporter = resolve(importer);
        const isAuthoredEntrypoint = resolvedImporter === resolve(intermediate);
        const isAuthoredModule = resolvedImporter.startsWith(`${resolve(authoredRoot)}/`);
        if ((isAuthoredEntrypoint || isAuthoredModule) &&
            !source.startsWith(".") && !source.startsWith("/")) {
          throw new Error(`unapproved bare import from authored module: ${source}`);
        }
        if (isAuthoredEntrypoint) {
          return resolveAuthoredModule(authoredRoot, dirname(input), source);
        }
        if (!isAuthoredModule) return null;
        return resolveAuthoredModule(authoredRoot, dirname(resolvedImporter), source);
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

async function resolveAuthoredModule(root: string, from: string, source: string): Promise<string> {
  const realRoot = await realpath(root);
  const raw = resolve(from, source);
  for (const candidate of [raw, raw.replace(/\.js$/, ".ts"), raw.replace(/\.js$/, ".tsx"), `${raw}.ts`, `${raw}.tsx`, join(raw, "index.ts"), join(raw, "index.tsx")]) {
    try {
      await assertNoSymlinkBelow(realRoot, candidate);
      const resolved = await realpath(candidate);
      if (!resolved.startsWith(`${realRoot}/`) || !/\.(?:ts|tsx|js|mjs)$/.test(resolved)) throw new Error(`authored import escapes rootDir: ${source}`);
      return resolved;
    } catch (error) {
      if (error instanceof Error && error.message.includes("escapes rootDir")) throw error;
    }
  }
  throw new Error(`cannot resolve authored import ${source}`);
}

async function assertNoSymlinkBelow(root: string, target: string): Promise<void> {
  const relativePath = relative(root, target);
  if (relativePath.startsWith("..") || relativePath === "") throw new Error("authored import escapes rootDir");
  let cursor = root;
  for (const component of relativePath.split("/")) {
    cursor = join(cursor, component);
    try { if ((await lstat(cursor)).isSymbolicLink()) throw new Error("authored import escapes rootDir"); } catch (error) {
      if (error instanceof Error && error.message.includes("escapes rootDir")) throw error;
      break;
    }
  }
}

async function rewriteRelativeImports(code: string, sourcePath: string): Promise<string> {
  const sourceDirectory = dirname(sourcePath);
  const pattern = /((?:from\s*|import\s*\()(["']))(\.{1,2}\/[\s\S]*?)(\2)/g;
  const matches = [...code.matchAll(pattern)];
  let rewritten = code;
  for (const match of matches) {
    const specifier = match[3] ?? "";
    const absolute = await resolveSourceImport(sourceDirectory, specifier);
    rewritten = rewritten.replace(match[0], `${match[1]}${absolute}${match[4]}`);
  }
  return rewritten;
}

async function resolveSourceImport(sourceDirectory: string, specifier: string): Promise<string> {
  const candidate = resolve(sourceDirectory, specifier);
  for (const path of [candidate, candidate.replace(/\.js$/, ".ts"), candidate.replace(/\.js$/, ".tsx")]) {
    try {
      await access(path);
      if ((await lstat(path)).isSymbolicLink()) throw new Error(`authored import is a symlink: ${specifier}`);
      if (path !== sourceDirectory && !path.startsWith(`${sourceDirectory}/`)) {
        throw new Error(`authored import escapes rootDir: ${specifier}`);
      }
      return path;
    } catch (error) {
      if (error instanceof Error && error.message.includes("escapes rootDir")) throw error;
    }
  }
  throw new Error(`cannot resolve authored relative import ${specifier}`);
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
