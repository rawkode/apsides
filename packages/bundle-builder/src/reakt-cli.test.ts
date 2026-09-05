import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

for (const [file, args, code, diagnostic] of [
  ["reakt.ts", [], 2, "usage: reakt.ts SOURCE OUTPUT"],
  ["reakt-lock.ts", [], 2, "usage: reakt-lock.ts ENTRY [MODULE ...]"],
  ["reakt.ts", ["/nonexistent-private-canary/source.ts", "/nonexistent-private-canary/output"], 1, "reakt: source compilation failed"],
  ["reakt-lock.ts", ["/nonexistent-private-canary/source.ts"], 1, "reakt: source lock generation failed"],
] as const) {
  const result = spawnSync(process.execPath, [fileURLToPath(new URL(file, import.meta.url)), ...args], { encoding: "utf8" });
  assert.equal(result.error, undefined);
  assert.equal(result.status, code);
  assert.equal(result.stdout, "");
  assert.equal(result.stderr.trim(), diagnostic);
  assert.ok(!result.stderr.includes("private-canary"));
}
console.log("Source CLI failures return bounded diagnostics and exit codes without exception traces.");
