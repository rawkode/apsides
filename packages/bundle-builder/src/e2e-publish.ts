import { createHash } from "node:crypto";
import { access, lstat, mkdtemp, readFile, realpath, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve } from "node:path";
import { buildBundle, type BuiltBundle, type BundleIntegrityManifestV0 } from "./index.js";

export interface E2EBundleMetadata {
  readonly metadataVersion: "oxideloop.dev/e2e-bundle-metadata/v0";
  readonly buildBundle: {
    /** SHA-256 of the canonical integrity-manifest.json bytes. */
    readonly bundleDigest: string;
    readonly bundleDigestScope: "canonical integrity-manifest.json sha256";
    /** Consumers must require this metadata commit marker and verify this SHA-256 before use. */
    readonly archive: { readonly byteLength: number; readonly sha256: string };
    readonly actionModule: { readonly identity: string; readonly digest: string };
    readonly renderModule: { readonly identity: string; readonly digest: string };
    readonly modules: {
      readonly action: { readonly identity: string; readonly digest: string };
      readonly render: { readonly identity: string; readonly digest: string };
    };
    readonly compiler: { readonly identity: string; readonly version: string };
    readonly bundler: { readonly identity: string; readonly version: string };
    readonly rendererAbiVersion: string;
    readonly authoredSource: {
      readonly location: string;
      readonly action: string;
      readonly render: string;
    };
  };
}

export interface PublishE2EBundleOptions {
  readonly repositoryRoot: string;
  readonly outPath: string;
  readonly metadataPath: string;
  readonly sourceRoot?: string;
  readonly build?: (options: { rootDir: string }) => Promise<BuiltBundle>;
  /** Test-only fault injection for the two final publication renames. */
  readonly renameFile?: (source: string, destination: string) => Promise<void>;
}

export interface PublishedE2EBundle {
  readonly archivePath: string;
  readonly metadataPath: string;
  readonly metadata: E2EBundleMetadata;
}

/**
 * Build and publish the E2E bundle and its inspection metadata.
 *
 * Publication uses cooperative single-writer semantics: callers must provide
 * fresh destinations, and consumers must require the metadata commit marker
 * and verify its archive SHA-256 before using the archive. Both files are
 * completely staged before either destination is created. The archive is
 * installed first and the metadata rename is the commit marker. If that final
 * rename fails, the newly-created archive is removed again.
 */
export async function publishE2EBundle(options: PublishE2EBundleOptions): Promise<PublishedE2EBundle> {
  const repositoryRoot = await realpath(resolve(options.repositoryRoot));
  const sourceRoot = await realpath(resolve(options.sourceRoot ?? join(repositoryRoot, "packages/example-bundle/src")));
  assertContained(repositoryRoot, sourceRoot, "authored source");
  const out = await prepareDestination(options.outPath);
  const metadataDestination = await prepareDestination(options.metadataPath);
  if (out.canonical === metadataDestination.canonical) throw new Error("--out and --metadata must name different files");

  const build = options.build ?? ((buildOptions: { rootDir: string }) => buildBundle(buildOptions));
  const result = await build({ rootDir: sourceRoot });
  const builtMetadata = createMetadata(result, repositoryRoot, sourceRoot);
  const metadataBytes = Buffer.from(`${JSON.stringify(builtMetadata, null, 2)}\n`, "utf8");

  await stageAndCommit(out, result.archiveBytes, metadataDestination, metadataBytes, options.renameFile ?? rename);
  return { archivePath: out.requested, metadataPath: metadataDestination.requested, metadata: builtMetadata };
}

function createMetadata(result: BuiltBundle, repositoryRoot: string, sourceRoot: string): E2EBundleMetadata {
  const action = moduleFor(result.manifest, "action");
  const render = moduleFor(result.manifest, "render");
  const sourceLocation = repositoryRelative(repositoryRoot, sourceRoot);
  const actionSource = `${sourceLocation}/action.ts`;
  const renderSource = `${sourceLocation}/render.tsx`;
  return {
    metadataVersion: "oxideloop.dev/e2e-bundle-metadata/v0",
    buildBundle: {
      bundleDigest: result.bundleDigest,
      bundleDigestScope: "canonical integrity-manifest.json sha256",
      archive: {
        byteLength: result.archiveBytes.byteLength,
        sha256: sha256(result.archiveBytes),
      },
      actionModule: { identity: action.identity, digest: action.sha256 },
      renderModule: { identity: render.identity, digest: render.sha256 },
      modules: {
        action: { identity: action.identity, digest: action.sha256 },
        render: { identity: render.identity, digest: render.sha256 },
      },
      compiler: result.manifest.compiler,
      bundler: result.manifest.bundler,
      rendererAbiVersion: result.manifest.rendererAbiVersion,
      authoredSource: { location: sourceLocation, action: actionSource, render: renderSource },
    },
  };
}

function moduleFor(manifest: BundleIntegrityManifestV0, phase: "action" | "render") {
  const modules = phase === "action" ? manifest.actionModules : manifest.renderModules;
  const module = modules[0];
  if (!module) throw new Error(`bundle manifest has no ${phase} module`);
  return module;
}

function repositoryRelative(repositoryRoot: string, path: string): string {
  const value = relative(repositoryRoot, path).replaceAll("\\", "/");
  if (value === "" || value.startsWith("../") || value === "..") throw new Error(`authored source must be inside repository root: ${path}`);
  return value;
}

interface ResolvedDestination {
  readonly requested: string;
  readonly canonical: string;
  readonly parent: string;
}

async function prepareDestination(path: string): Promise<ResolvedDestination> {
  const requested = resolve(path);
  const lexicalParent = dirname(requested);
  const parentInfo = await lstat(lexicalParent);
  if (!parentInfo.isDirectory()) throw new Error(`output parent is not a directory: ${lexicalParent}`);
  const parent = await realpath(lexicalParent);
  const canonical = join(parent, basename(requested));
  await assertAbsent(canonical);
  return { requested, canonical, parent };
}

async function assertAbsent(path: string): Promise<void> {
  try {
    await lstat(path);
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") return;
    throw error;
  }
  throw new Error(`output destination already exists: ${path}`);
}

function assertContained(root: string, child: string, label: string): void {
  const value = relative(root, child);
  if (value === "" || value.startsWith("..") || value === "..") throw new Error(`${label} must be inside repository root: ${child}`);
}

async function stageAndCommit(
  archive: ResolvedDestination,
  archiveBytes: Uint8Array,
  metadata: ResolvedDestination,
  metadataBytes: Uint8Array,
  renameFile: (source: string, destination: string) => Promise<void>,
): Promise<void> {
  const staged: Array<{ destination: ResolvedDestination; temporary: string }> = [];
  const temporaryDirectories: string[] = [];
  let archiveCommitted = false;
  let metadataCommitted = false;
  try {
    for (const file of [
      { destination: archive, bytes: archiveBytes },
      { destination: metadata, bytes: metadataBytes },
    ]) {
      await access(file.destination.parent);
      const temporaryDirectory = await mkdtemp(join(file.destination.parent, ".oxideloop-e2e-"));
      temporaryDirectories.push(temporaryDirectory);
      const temporary = join(temporaryDirectory, "output");
      await writeFile(temporary, file.bytes, { flag: "wx" });
      staged.push({ destination: file.destination, temporary });
    }
    await assertAbsent(archive.canonical);
    await renameFile(staged[0]!.temporary, archive.canonical);
    archiveCommitted = true;
    await assertAbsent(metadata.canonical);
    // This is the final commit marker. A successful return means both files
    // exist; a failure removes the archive created immediately above.
    await renameFile(staged[1]!.temporary, metadata.canonical);
    metadataCommitted = true;
  } finally {
    if (archiveCommitted && !metadataCommitted) await rm(archive.canonical, { force: true });
    await Promise.all(temporaryDirectories.map((directory) => rm(directory, { recursive: true, force: true })));
  }
}

function sha256(value: Uint8Array): string {
  return createHash("sha256").update(value).digest("hex");
}

export async function readPublishedMetadata(path: string): Promise<E2EBundleMetadata> {
  return JSON.parse(await readFile(path, "utf8")) as E2EBundleMetadata;
}
