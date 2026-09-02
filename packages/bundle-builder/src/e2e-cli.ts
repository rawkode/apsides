import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { publishE2EBundle } from "./e2e-publish.js";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

function usage(): string {
  return "Usage: pnpm bundle:e2e -- --out <archive> --metadata <metadata.json> [--source <controller-directory>]";
}

function parseArgs(args: readonly string[]): { outPath: string; metadataPath: string; sourceRoot: string } {
  let outPath: string | undefined;
  let metadataPath: string | undefined;
  let sourceRoot: string | undefined;
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--") continue;
    if (argument === "--help" || argument === "-h") {
      console.log(usage());
      process.exit(0);
    }
    if (argument === "--out") outPath = args[++index];
    else if (argument === "--metadata") metadataPath = args[++index];
    else if (argument === "--source") sourceRoot = args[++index];
    else throw new Error(`unknown or incomplete argument: ${argument}`);
  }
  if (!outPath || !metadataPath) throw new Error(usage());
  return {
    outPath: resolve(repositoryRoot, outPath),
    metadataPath: resolve(repositoryRoot, metadataPath),
    sourceRoot: resolve(repositoryRoot, sourceRoot ?? "packages/example-bundle/src"),
  };
}

try {
  const { outPath, metadataPath, sourceRoot } = parseArgs(process.argv.slice(2));
  await publishE2EBundle({
    repositoryRoot,
    outPath,
    metadataPath,
    sourceRoot,
  });
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
