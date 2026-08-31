import { execFile as execFileCallback } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { encodeBundleArchive } from "./archive.js";
import { buildBundle } from "./index.js";

const execFile = promisify(execFileCallback);
const parent = await mkdtemp(join(tmpdir(), "oxideloop-builder-test-"));
const packageRoot = resolve(fileURLToPath(new URL("../", import.meta.url)));
const names = ["action.callable.js", "bundle-install.json", "descriptor.json", "integrity-manifest.json", "render.callable.js"] as const;

async function writeAuthoredFixture(root: string, render: string, extra: Record<string, string> = {}): Promise<void> {
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "action.ts"), "export const action = () => ({ abiVersion: 'renderer.oxideloop.dev/v0', actions: [], diagnostics: [] });\n");
  await writeFile(join(root, "render.tsx"), render);
  await Promise.all(Object.entries(extra).map(async ([name, source]) => {
    const path = join(root, name);
    await mkdir(resolve(path, ".."), { recursive: true });
    await writeFile(path, source);
  }));
}

async function mustRejectBundle(root: string, reason: string): Promise<void> {
  const result = await Promise.allSettled([buildBundle({ rootDir: root })]);
  if (result[0]?.status !== "rejected") throw new Error(`${reason}: bundle unexpectedly succeeded`);
}

function archiveEntry(archive: Uint8Array, wanted: string): Buffer {
  if (Buffer.from(archive.subarray(0, 4)).toString("ascii") !== "OLBA" || archive[4] !== 1) throw new Error("invalid bundle archive");
  const count = archive[5];
  if (count !== 5) throw new Error(`expected five archive entries, received ${count}`);
  let offset = 6;
  const headers: Array<{ name: string; length: number }> = [];
  for (let index = 0; index < count; index += 1) {
    const nameLength = archive[offset++];
    if (nameLength === undefined) throw new Error("truncated archive entry name");
    const name = Buffer.from(archive.subarray(offset, offset + nameLength)).toString("utf8");
    offset += nameLength;
    const length = Buffer.from(archive.subarray(offset, offset + 4)).readUInt32BE();
    offset += 4 + 64; // frozen lowercase SHA-256 header
    headers.push({ name, length });
  }
  for (const header of headers) {
    const bytes = Buffer.from(archive.subarray(offset, offset + header.length));
    if (header.name === wanted) return bytes;
    offset += header.length;
  }
  throw new Error(`missing archive entry: ${wanted}`);
}

function entries(value: Uint8Array = new Uint8Array([1])) {
  return Object.fromEntries(names.map((name) => [name, value])) as Record<(typeof names)[number], Uint8Array>;
}

try {
  const workspaceRoot = resolve(fileURLToPath(new URL("../../../", import.meta.url)));
  const rootDir = resolve(fileURLToPath(new URL("../../example-bundle/src/", import.meta.url)));
  const [first, second] = await Promise.all([buildBundle({ rootDir }), buildBundle({ rootDir })]);
  if (!first.archiveBytes.equals(second.archiveBytes)) throw new Error("archive bytes must be deterministic");
  // P7's deterministic action metadata is part of the signed archive input.
  if (createHash("sha256").update(first.archiveBytes).digest("hex") !== "283bb2a6e7914cf1ed7a92a15f5685b2e5bdd7a4bb736cf30e874db24a415ce8") {
    throw new Error("archive golden bytes changed");
  }
  const original = first.archiveBytes.at(0);
  if (original === undefined) throw new Error("archive cannot be empty");
  first.archiveBytes[0] = original ^ 1;
  if ((await buildBundle({ rootDir })).archiveBytes[0] !== original) {
    throw new Error("returned archive bytes must not mutate later builds");
  }

  // These cases all drive the public builder through its Rolldown hooks; they
  // deliberately exercise filesystem and resolver policy rather than helpers.
  const authoredFixtures = await mkdtemp(join(packageRoot, ".bundle-builder-authored-"));
  try {
    const jsxRoot = join(authoredFixtures, "jsx");
    await writeAuthoredFixture(jsxRoot, "import { component } from './nested/component.js'; export const render = () => component();\n", {
      "nested/component.tsx": "export const component = () => <ConfigMap apiVersion=\"v1\" kind=\"ConfigMap\" metadata={{name: 'nested'}} />;\n",
    });
    const jsxBundle = await buildBundle({ rootDir: jsxRoot });
    const nestedRender = archiveEntry(jsxBundle.archiveBytes, "render.callable.js").toString("utf8");
    // This inspects the emitted callable, not merely Oxc's successful parse:
    // automatic TSX lowering is bundled with OxideLoop's runtime and preserves
    // the authored ConfigMap semantics without a React dependency.
    if (nestedRender.toLowerCase().includes("react")) {
      throw new Error("nested TSX bundle retained a React runtime dependency");
    }
    if (!nestedRender.includes("ConfigMap") || !nestedRender.includes("nested")) {
      throw new Error("nested TSX bundle did not preserve OxideLoop resource semantics");
    }

    // macOS often exposes the same files through /var and /private/var. The
    // builder accepts the caller-selected root by canonical identity, rather
    // than comparing string prefixes across that alias boundary.
    const aliasedTarget = await mkdtemp(join(tmpdir(), "oxideloop-var-private-"));
    const aliasedRoot = join(authoredFixtures, "var-private-alias");
    try {
      await writeAuthoredFixture(aliasedTarget, "export const render = () => null;\n");
      await symlink(aliasedTarget, aliasedRoot);
      await buildBundle({ rootDir: aliasedRoot });
    } finally {
      await rm(aliasedTarget, { recursive: true, force: true });
    }

    const staleRoot = join(authoredFixtures, "stale-js");
    await writeAuthoredFixture(staleRoot, "import { value } from './value.js'; export const render = () => value;\n", {
      "value.ts": "export const value = 1;\n",
      "value.js": "throw new Error('stale JavaScript must not be loaded');\n",
    });
    await buildBundle({ rootDir: staleRoot }); // .js specifiers strictly select authored .ts/.tsx.

    const traversalRoot = join(authoredFixtures, "traversal", "nested");
    await writeAuthoredFixture(traversalRoot, "import '../outside.js'; export const render = () => null;\n");
    await writeFile(join(authoredFixtures, "traversal", "outside.ts"), "export const outside = 1;\n");
    await mustRejectBundle(traversalRoot, "nested traversal");

    const absoluteRoot = join(authoredFixtures, "absolute");
    await writeAuthoredFixture(absoluteRoot, "import '/tmp/forbidden.js'; export const render = () => null;\n");
    await mustRejectBundle(absoluteRoot, "absolute import");

    const rawRoot = join(authoredFixtures, "raw");
    await writeAuthoredFixture(rawRoot, "import './raw.mjs'; export const render = () => null;\n", { "raw.mjs": "export default null;\n" });
    await mustRejectBundle(rawRoot, "raw MJS import");

    const bareRoot = join(authoredFixtures, "bare");
    await writeAuthoredFixture(bareRoot, "import 'node:fs'; export const render = () => null;\n");
    await mustRejectBundle(bareRoot, "bare fallback import");

    const literalDynamicRoot = join(authoredFixtures, "literal-dynamic");
    await writeAuthoredFixture(literalDynamicRoot, "export const render = () => import('./next.js');\n", { "next.ts": "export const next = 1;\n" });
    await mustRejectBundle(literalDynamicRoot, "literal dynamic import");

    const expressionDynamicRoot = join(authoredFixtures, "expression-dynamic");
    await writeAuthoredFixture(expressionDynamicRoot, "const name = './next.js'; export const render = () => import(name);\n", { "next.ts": "export const next = 1;\n" });
    await mustRejectBundle(expressionDynamicRoot, "expression dynamic import");

    const syntaxTextRoot = join(authoredFixtures, "syntax-text");
    await writeAuthoredFixture(syntaxTextRoot, "const text = 'import(\\\"not syntax\\\")'; const template = `import(${text})`; const pattern = /import\\s*\\(/; /* import('./comment.js') */ export const render = () => [text, template, pattern.source];\n");
    await buildBundle({ rootDir: syntaxTextRoot });

    const outside = join(authoredFixtures, "outside");
    await mkdir(outside);
    await writeFile(join(outside, "file.ts"), "export const escaped = 1;\n");
    const fileLinkRoot = join(authoredFixtures, "file-link");
    await writeAuthoredFixture(fileLinkRoot, "import './escape.js'; export const render = () => null;\n");
    await symlink(join(outside, "file.ts"), join(fileLinkRoot, "escape.ts"));
    await mustRejectBundle(fileLinkRoot, "symlinked file");

    const directoryLinkRoot = join(authoredFixtures, "directory-link");
    await writeAuthoredFixture(directoryLinkRoot, "import './escape/file.js'; export const render = () => null;\n");
    await symlink(outside, join(directoryLinkRoot, "escape"));
    await mustRejectBundle(directoryLinkRoot, "symlinked directory");
  } finally {
    await rm(authoredFixtures, { recursive: true, force: true });
  }
  const alternating = entries();
  let reads = 0;
  Object.defineProperty(alternating, "action.callable.js", { enumerable: true, get: () => { reads += 1; return new Uint8Array([reads]); } });
  const alternatingArchive = encodeBundleArchive(alternating);
  if (alternatingArchive[alternatingArchive.length - names.length] !== 1 || reads !== 1) {
    throw new Error("encoder must snapshot each getter exactly once");
  }
  const headerBytes = 6 + names.reduce((total, name) => total + 1 + name.length + 4 + 64, 0);
  const atLimit = entries(new Uint8Array(1));
  atLimit["action.callable.js"] = new Uint8Array(128 * 1024 - headerBytes - (names.length - 1));
  if (encodeBundleArchive(atLimit).byteLength !== 128 * 1024) throw new Error("encoder must accept the inclusive total limit");
  atLimit["action.callable.js"] = new Uint8Array(atLimit["action.callable.js"].byteLength + 1);
  if ((await Promise.allSettled([Promise.resolve().then(() => encodeBundleArchive(atLimit))]))[0]?.status !== "rejected") {
    throw new Error("encoder must reject one byte beyond the total limit");
  }
  class LyingLargeArray extends Uint8Array { get byteLength() { return 1; } }
  for (const hostile of [
    entries(new LyingLargeArray(1024 * 1024)),
    entries(new Proxy(new Uint8Array([1]), {})),
    ...(typeof SharedArrayBuffer === "undefined" ? [] : [entries(new Uint8Array(new SharedArrayBuffer(1)))]),
  ]) {
    if ((await Promise.allSettled([Promise.resolve().then(() => encodeBundleArchive(hostile))]))[0]?.status !== "rejected") {
      throw new Error("encoder accepted a hostile typed-array input");
    }
  }
  if (typeof SharedArrayBuffer !== "undefined") {
    const spoofed = new SharedArrayBuffer(1);
    Object.defineProperty(spoofed, Symbol.toStringTag, { value: "ArrayBuffer" });
    if ((await Promise.allSettled([Promise.resolve().then(() => encodeBundleArchive(entries(new Uint8Array(spoofed))))]))[0]?.status !== "rejected") {
      throw new Error("encoder accepted a Symbol.toStringTag-spoofed shared buffer");
    }
  }
  const fixture = join(parent, "typescript.olba");
  await writeFile(fixture, second.archiveBytes, { flag: "wx" });
  await execFile("cargo", ["test", "-p", "oxideloop-bundle-archive", "--test", "builder_output", "--locked", "--", "--ignored", "--exact", "typescript_archive_installs_and_opens_by_its_raw_manifest_digest"], {
    cwd: workspaceRoot,
    env: { ...process.env, OXIDELOOP_BUILT_ARCHIVE: fixture, OXIDELOOP_EXPECTED_BUNDLE_DIGEST: second.bundleDigest },
  });
} finally {
  await rm(parent, { recursive: true, force: true });
}
