import { execFile as execFileCallback } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { encodeBundleArchive } from "./archive.js";
import { buildBundle } from "./index.js";

const execFile = promisify(execFileCallback);
const parent = await mkdtemp(join(tmpdir(), "oxideloop-builder-test-"));
const names = ["action.callable.js", "bundle-install.json", "descriptor.json", "integrity-manifest.json", "render.callable.js"] as const;

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
