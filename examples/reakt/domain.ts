import { buildProgram } from "./program.ts";

const programUrl = new URL("./program.ts", import.meta.url);
const bytes = await Deno.readFile(programUrl);
const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
const sourceDigest = Array.from(hash, (byte) => byte.toString(16).padStart(2, "0")).join("");

export const { productionIR, fixtureIR, fixtures } = buildProgram(sourceDigest);

if (import.meta.main) {
  const mode = Deno.args[0] ?? "--production";
  const output = mode === "--production" ? productionIR : mode === "--fixture" ? fixtureIR : mode === "--fixture-bundle" ? fixtures : undefined;
  if (output === undefined) throw new Error("usage: domain.ts [--production|--fixture|--fixture-bundle]");
  console.log(JSON.stringify(output, null, 2));
}
