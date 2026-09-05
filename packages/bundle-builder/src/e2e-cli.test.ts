import { execFile as execFileCallback } from "node:child_process";
import { createHash } from "node:crypto";
import { access, mkdir, mkdtemp, readFile, rename, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join, relative, resolve } from "node:path";
import { promisify } from "node:util";
import { buildBundle } from "./index.js";
import { publishE2EBundle, readPublishedMetadata } from "./e2e-publish.js";

const execFile = promisify(execFileCallback);
const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(packageRoot, "../..");
const cliPath = join(packageRoot, "src/e2e-cli.ts");
const tsxPath = join(packageRoot, "node_modules/.bin/tsx");
const temporaryRoot = await mkdtemp(join(tmpdir(), "oxideloop-e2e-cli-test-"));
const sourceRoot = await mkdtemp(join(packageRoot, ".bundle-builder-e2e-source-"));
await mkdir(join(repositoryRoot, "work"), { recursive: true });
const relativeOutputRoot = await mkdtemp(join(repositoryRoot, "work/bundle-builder-cli-test-"));

await writeFile(join(sourceRoot, "controller.tsx"), `
import { Event, defineAction, defineController } from "@oxideloop/kubernetes";
const configurationChanged = defineAction({
  id: "configuration-changed",
  version: "v1",
  run: ({ source, metadata }) => <Event id="notification" regarding={source} reason="ConfigurationChanged" action="ConfigurationChanged" note={metadata.message} />,
});
export default defineController(({ root }) => root.metadata.name);
`);

async function mustNotExist(path: string): Promise<void> {
  const result = await Promise.allSettled([access(path)]);
  if (result[0]?.status === "fulfilled") throw new Error(`unexpected output exists: ${path}`);
}

try {
  const firstArchive = join(temporaryRoot, "first.olba");
  const firstMetadata = join(temporaryRoot, "first.json");
  const secondArchive = join(temporaryRoot, "second.olba");
  const secondMetadata = join(temporaryRoot, "second.json");
  const cliArgs = (archive: string, metadata: string) => ["--out", archive, "--metadata", metadata, "--source", sourceRoot];

  // Run from both the repository and an unrelated directory: source resolution
  // is explicit and must not depend on the caller's working directory.
  await execFile(tsxPath, [cliPath, ...cliArgs(firstArchive, firstMetadata)], { cwd: repositoryRoot });
  await execFile(tsxPath, [cliPath, ...cliArgs(secondArchive, secondMetadata)], { cwd: temporaryRoot });
  const firstBytes = await readFile(firstArchive);
  const secondBytes = await readFile(secondArchive);
  if (!firstBytes.equals(secondBytes)) throw new Error("two E2E builds must have byte-identical archives");

  const first = await readPublishedMetadata(firstMetadata);
  const second = await readPublishedMetadata(secondMetadata);
  const firstMetadataBytes = await readFile(firstMetadata);
  if (first.buildBundle.bundleDigest !== second.buildBundle.bundleDigest) {
    throw new Error("two E2E builds must have equal bundle digests");
  }
  if (first.buildBundle.archive.byteLength !== firstBytes.byteLength) throw new Error("metadata archive length is incorrect");
  const archiveSha = createHash("sha256").update(firstBytes).digest("hex");
  if (first.buildBundle.archive.sha256 !== archiveSha) throw new Error("metadata archive SHA-256 is incorrect");
  if (first.buildBundle.bundleDigest === first.buildBundle.archive.sha256) {
    throw new Error("integrity-manifest digest must be distinct from archive SHA-256");
  }

  const built = await buildBundle({ rootDir: sourceRoot });
  const action = built.manifest.actionModules[0];
  const render = built.manifest.renderModules[0];
  if (!action || !render) throw new Error("fixture manifest is missing a module");
  if (first.buildBundle.bundleDigest !== built.bundleDigest) throw new Error("metadata bundle digest is incorrect");
  if (first.buildBundle.sourceSha256 !== built.sourceSha256) throw new Error("metadata source SHA-256 is incorrect");
  if (first.buildBundle.actionModule.identity !== action.identity || first.buildBundle.actionModule.digest !== action.sha256) {
    throw new Error("metadata action module integrity is incorrect");
  }
  if (first.buildBundle.renderModule.identity !== render.identity || first.buildBundle.renderModule.digest !== render.sha256) {
    throw new Error("metadata render module integrity is incorrect");
  }
  if (JSON.stringify(first.buildBundle.compiler) !== JSON.stringify(built.manifest.compiler)) throw new Error("metadata compiler is incorrect");
  if (JSON.stringify(first.buildBundle.bundler) !== JSON.stringify(built.manifest.bundler)) throw new Error("metadata bundler is incorrect");
  if (first.buildBundle.rendererAbiVersion !== built.manifest.rendererAbiVersion) throw new Error("metadata ABI version is incorrect");
  if (!first.buildBundle.authoredSource.location.startsWith("packages/bundle-builder/.bundle-builder-e2e-source-")) {
    throw new Error("metadata authored source location is incorrect");
  }
  if (`${first.buildBundle.authoredSource.location}/controller.tsx` !== first.buildBundle.authoredSource.controller) {
    throw new Error("metadata controller source location is incorrect");
  }

  // The documented root-level command is executed by pnpm with the package as
  // its cwd. Relative destinations must still be repository-relative.
  const relativeArchive = join(relativeOutputRoot, "relative.olba");
  const relativeMetadata = join(relativeOutputRoot, "relative.json");
  await execFile(
    tsxPath,
    [cliPath, ...cliArgs(relative(repositoryRoot, relativeArchive), relative(repositoryRoot, relativeMetadata))],
    { cwd: temporaryRoot },
  );
  await access(relativeArchive);
  await access(relativeMetadata);

  let existingBuildCalled = false;
  const existing = await Promise.allSettled([publishE2EBundle({
    repositoryRoot,
    outPath: firstArchive,
    metadataPath: firstMetadata,
    build: async () => {
      existingBuildCalled = true;
      throw new Error("existing destinations must be rejected before build");
    },
  })]);
  if (existing[0]?.status !== "rejected") throw new Error("existing destinations must be refused");
  if (existingBuildCalled) throw new Error("existing destinations were rejected after build");
  if (!(await readFile(firstArchive)).equals(firstBytes)) throw new Error("existing archive must remain unchanged");
  if (!(await readFile(firstMetadata)).equals(firstMetadataBytes)) throw new Error("existing metadata changed");

  const failedArchive = join(temporaryRoot, "failed.olba");
  const failedMetadata = join(temporaryRoot, "failed.json");
  await Promise.allSettled([
    publishE2EBundle({
      repositoryRoot,
      outPath: failedArchive,
      metadataPath: failedMetadata,
      build: async () => { throw new Error("intentional build failure"); },
    }),
  ]);
  await mustNotExist(failedArchive);
  await mustNotExist(failedMetadata);

  let renameCount = 0;
  const renameFailureArchive = join(temporaryRoot, "rename-failure.olba");
  const renameFailureMetadata = join(temporaryRoot, "rename-failure.json");
  const renameFailure = await Promise.allSettled([publishE2EBundle({
    repositoryRoot,
    sourceRoot,
    outPath: renameFailureArchive,
    metadataPath: renameFailureMetadata,
    renameFile: async (source, destination) => {
      renameCount += 1;
      if (renameCount === 2) throw new Error("intentional metadata commit-marker failure");
      await rename(source, destination);
    },
  })]);
  if (renameFailure[0]?.status !== "rejected" || renameCount !== 2) throw new Error("second rename failure was not exercised");
  await mustNotExist(renameFailureArchive);
  await mustNotExist(renameFailureMetadata);

  // A destination validation failure must not leave the first destination
  // published after its companion destination was rejected.
  const blockedArchive = join(temporaryRoot, "blocked.olba");
  const blockedMetadata = join(temporaryRoot, "blocked-metadata");
  await mkdir(blockedMetadata);
  const blocked = await Promise.allSettled([publishE2EBundle({
    repositoryRoot,
    outPath: blockedArchive,
    metadataPath: blockedMetadata,
  })]);
  if (blocked[0]?.status !== "rejected") throw new Error("directory metadata destination must fail");
  await mustNotExist(blockedArchive);

  const aliasRealParent = join(temporaryRoot, "alias-real");
  const aliasParent = join(temporaryRoot, "alias-link");
  await mkdir(aliasRealParent);
  await symlink(aliasRealParent, aliasParent);
  let aliasBuildCalled = false;
  const aliasFailure = await Promise.allSettled([publishE2EBundle({
    repositoryRoot,
    outPath: join(aliasRealParent, "same-output"),
    metadataPath: join(aliasParent, "same-output"),
    build: async () => {
      aliasBuildCalled = true;
      throw new Error("aliased destinations must be rejected before build");
    },
  })]);
  if (aliasFailure[0]?.status !== "rejected") throw new Error("symlink-aliased destinations must be rejected");
  if (aliasBuildCalled) throw new Error("symlink-aliased destinations were rejected after build");

  const externalSource = await mkdtemp(join(tmpdir(), "oxideloop-external-source-"));
  const escapedSource = join(temporaryRoot, "escaped-source");
  await symlink(externalSource, escapedSource);
  let sourceBuildCalled = false;
  const sourceFailure = await Promise.allSettled([publishE2EBundle({
    repositoryRoot,
    sourceRoot: escapedSource,
    outPath: join(temporaryRoot, "escaped.olba"),
    metadataPath: join(temporaryRoot, "escaped.json"),
    build: async () => {
      sourceBuildCalled = true;
      throw new Error("escaped source must be rejected before build");
    },
  })]);
  if (sourceFailure[0]?.status !== "rejected") throw new Error("source symlink escape must be rejected");
  if (sourceBuildCalled) throw new Error("source symlink escape was rejected after build");
  await rm(externalSource, { recursive: true, force: true });
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
  await rm(sourceRoot, { recursive: true, force: true });
  await rm(relativeOutputRoot, { recursive: true, force: true });
}
