import { mkdir, mkdtemp, readdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { execFile as execFileCallback } from "node:child_process";
import { tmpdir } from "node:os";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { buildBundle } from "./index.js";

const execFile = promisify(execFileCallback);
const parent = await mkdtemp(join(tmpdir(), "oxideloop-builder-test-"));
try {
  const workspaceRoot = resolve(fileURLToPath(new URL("../../../", import.meta.url)));
  const rootDir = resolve(fileURLToPath(new URL("../../example-bundle/src/", import.meta.url)));
  const [first, second] = await Promise.all([
    buildBundle({ rootDir, outDir: join(parent, "first") }),
    buildBundle({ rootDir, outDir: join(parent, "second") }),
  ]);
  const collisionOut = join(parent, "collision");
  const collision = await Promise.allSettled([
    buildBundle({ rootDir, outDir: collisionOut, compilerVersion: "one" }),
    buildBundle({ rootDir, outDir: collisionOut, compilerVersion: "two" }),
  ]);
  if (collision.filter((result) => result.status === "fulfilled").length !== 1 ||
      collision.filter((result) => result.status === "rejected").length !== 1) {
    throw new Error("same-output publishers must have one winner and one clean rejection");
  }
  const winner = collision.find((result): result is PromiseFulfilledResult<Awaited<ReturnType<typeof buildBundle>>> => result.status === "fulfilled")?.value;
  if (!winner || !(await readdir(collisionOut)).includes("render.callable.js") ||
      !(await readdir(winner.metadataDir)).includes("integrity-manifest.json")) {
    throw new Error("collision winner must publish a complete output/metadata pair");
  }
  for (const invalid of [
    buildBundle({ rootDir, outDir: rootDir }),
    buildBundle({ rootDir, outDir: join(parent, "safe"), metadataDir: join(rootDir, "metadata") }),
  ]) {
    if ((await Promise.allSettled([invalid]))[0]?.status !== "rejected") {
      throw new Error("builder must reject output or metadata overlap with authored sources");
    }
  }
  const hostileWorkspace = join(parent, "hostile-workspace");
  const hostileRoot = join(hostileWorkspace, "src");
  const outsidePackage = join(parent, "outside-evil");
  await mkdir(join(hostileRoot, "node_modules"), { recursive: true });
  await mkdir(outsidePackage, { recursive: true });
  await writeFile(join(hostileWorkspace, "pnpm-workspace.yaml"), "packages: []\n", "utf8");
  await writeFile(join(outsidePackage, "index.js"), "export const escaped = true;\n", "utf8");
  await symlink(outsidePackage, join(hostileRoot, "node_modules", "evil"));
  await writeFile(join(hostileRoot, "action.ts"), "export function action(input: unknown) { return input; }\n", "utf8");
  await writeFile(join(hostileRoot, "render.tsx"), "import 'evil'; export function render(input: unknown) { return input; }\n", "utf8");
  const hostileOut = join(parent, "hostile-out");
  const hostileResult = await Promise.allSettled([buildBundle({ rootDir: hostileRoot, outDir: hostileOut })]);
  if (hostileResult[0]?.status !== "rejected") {
    throw new Error("authored modules must not resolve unapproved bare imports");
  }
  const leakedEntries = new Set(await readdir(parent));
  for (const entry of ["hostile-out", ".hostile-out-metadata", ".hostile-out.publish-lock", "..hostile-out-metadata.publish-lock"]) {
    if (leakedEntries.has(entry)) throw new Error(`rejected bare import leaked publication artifact: ${entry}`);
  }
  if (!/^[0-9a-f]{64}$/.test(first.bundleDigest) || first.bundleDigest !== second.bundleDigest) {
    throw new Error("bundle digest must be reproducible raw SHA-256 hex");
  }
  const expected = ["action.callable.js", "descriptor.json", "render.callable.js"];
  for (const output of ["first", "second"]) {
    const entries = (await readdir(join(parent, output))).sort();
    if (JSON.stringify(entries) !== JSON.stringify(expected)) {
      throw new Error("installable output must contain exactly manifest-declared files");
    }
  }
  if ((await readFile(join(parent, "first", "render.callable.js"), "utf8")) !==
      (await readFile(join(parent, "second", "render.callable.js"), "utf8"))) {
    throw new Error("render callable bytes must be reproducible");
  }
  const action = await readFile(join(parent, "first", "action.callable.js"), "utf8");
  const render = await readFile(join(parent, "first", "render.callable.js"), "utf8");
  if (/(?:^|[^A-Za-z0-9_$])(?:process|Deno|require)\s*(?:[.(])/.test(`${action}\n${render}`)) {
    throw new Error("callables must not rely on Node or Deno ambient globals");
  }
  const renderCallable = Function(`"use strict"; return ${render}`)() as (input: Record<string, unknown>) => { automations: Array<{ bundleDigest: string; actionModuleDigest: string }> };
  const rendered = renderCallable({
    abiVersion: "renderer.oxideloop.dev/v0",
    root: { apiVersion: "demo.oxideloop.dev/v1alpha1", kind: "WebApplication", namespace: "demo", name: "portal", uid: "root-1", generation: 1, spec: { expose: true } },
    bundleDigest: first.bundleDigest,
    cacheEpoch: "0",
    observationToken: "0",
    observations: [{ apiVersion: "v1", kind: "ConfigMap", namespace: "demo", name: "demo-input", uid: "input-1", resourceVersion: "2", value: { data: { flavor: "vanilla" } } }],
  });
  const automation = rendered.automations[0];
  if (!automation || automation.bundleDigest !== first.bundleDigest ||
      automation.actionModuleDigest !== first.manifest.actionModules[0]?.sha256) {
    throw new Error("render execution must pin the final bundle and actual action module digest");
  }
  const actionCallable = Function(`"use strict"; return ${action}`)() as (input: Record<string, unknown>) => { actions: unknown[] };
  if (actionCallable({
    abiVersion: "renderer.oxideloop.dev/v0",
    bundleDigest: first.bundleDigest,
    automationIdentity: "demo.web-application.input-changed",
    automationVersion: "1",
    actionModuleIdentity: "demo.web-application.input-changed",
    actionModuleDigest: automation.actionModuleDigest,
    occurrenceId: "occ-1",
    sourceTransitionId: "transition-1",
    sourceUid: "input-1",
    sourceResourceVersion: "2",
    eventMetadata: { source: { namespace: "demo", name: "demo-input" }, note: "changed" },
  }).actions.length !== 1) throw new Error("action callable must adapt ActionInputV0 and execute the authored action");
  await execFile("cargo", ["test", "-p", "oxideloop-bundle-contract", "--test", "builder_output", "--locked", "--", "--ignored", "--exact", "builder_output_installs_and_opens_by_its_raw_manifest_digest"], {
    cwd: workspaceRoot,
    env: { ...process.env, OXIDELOOP_BUILT_BUNDLE_DIR: join(parent, "first"), OXIDELOOP_BUILT_METADATA_DIR: first.metadataDir },
  });
} finally {
  await rm(parent, { recursive: true, force: true });
}
