import assert from "node:assert/strict";
import { copyFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const directory = mkdtempSync(join(tmpdir(), "tsx-binary-smoke-"));
try {
  const binary = join(directory, "controller");
  copyFileSync(resolve("dist/controller"), binary);
  const child = Bun.spawn([binary, "plan", "--namespace", "standalone-test"], {
    cwd: directory, stdout: "pipe", stderr: "pipe",
  });
  const [code, stdout, stderr] = await Promise.all([
    child.exited, new Response(child.stdout).text(), new Response(child.stderr).text(),
  ]);
  assert.equal(code, 0, `${stdout}\n${stderr}`);
  const plan = JSON.parse(stdout);
  assert.equal(plan.permissions.roles[0].metadata.namespace, "standalone-test");
  assert.deepEqual(plan.permissions.cloud, []);
  assert.deepEqual(plan.resources.map((resource: { id: string }) => resource.id), ["nginx", "service"]);
  console.log("Standalone executable packaging passed. Live lifecycle acceptance: bun run e2e.");
} finally {
  rmSync(directory, { recursive: true, force: true });
}
