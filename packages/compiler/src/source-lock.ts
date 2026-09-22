import { createHash } from "node:crypto";
import { lstat, readFile, realpath, writeFile } from "node:fs/promises";
import { dirname, posix, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const sha256 = (value: Uint8Array): string => createHash("sha256").update(value).digest("hex");
const order = (left: string, right: string): number => left < right ? -1 : left > right ? 1 : 0;

/** Record an explicit source closure; dependency discovery never silently expands it. */
export const writeSourceLock = async (entryFile: string, moduleFiles: readonly string[] = []): Promise<string> => {
  const entry = await realpath(entryFile);
  if (!entry.endsWith(".tsx")) throw new Error(`source entry must be a .tsx program: ${entryFile}`);
  const root = dirname(entry);
  const files = [entry, ...await Promise.all(moduleFiles.map(file => realpath(file)))];
  const unique = [...new Set(files)];
  const inputs = await Promise.all(unique.map(async file => {
    if (file !== root && !file.startsWith(`${root}${sep}`)) throw new Error(`source module is outside entry directory: ${file}`);
    if (!(await lstat(file)).isFile()) throw new Error(`source module is not a regular file: ${file}`);
    const logical = posix.normalize(relative(root, file).split(sep).join("/"));
    if (!logical || logical.startsWith("../") || !/\.(ts|tsx|js|mjs)$/.test(logical)) throw new Error(`unsupported source module: ${file}`);
    return { path: logical, sha256: sha256(await readFile(file)) };
  }));
  inputs.sort((a,b) => order(a.path,b.path));
  const lock = { entry: relative(root, entry).split(sep).join("/"), inputs, schemaVersion: "apsides.dev/source-lock/v1" };
  const path = resolve(root, "apsides.lock.json");
  await writeFile(path, `${JSON.stringify(lock,null,2)}\n`);
  return path;
};

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [entry, ...modules] = process.argv.slice(2);
  if (!entry) {
    console.error("usage: source-lock.ts ENTRY [MODULE ...]");
    process.exitCode = 2;
  } else {
    try {
      console.log(await writeSourceLock(resolve(entry), modules.map(value => resolve(value))));
    } catch {
      console.error("apsides: source lock generation failed");
      process.exitCode = 1;
    }
  }
}
