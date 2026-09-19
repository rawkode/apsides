import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withControllerLock } from "../src/state";

test("OS releases the writer lock when a controller process is killed", async () => {
  const directory = mkdtempSync(join(tmpdir(), "tsx-lock-process-"));
  const module = new URL("../src/state.ts", import.meta.url).href;
  const child = Bun.spawn([process.execPath, "-e", `
    import { withControllerLock } from ${JSON.stringify(module)};
    await withControllerLock(${JSON.stringify(directory)}, async () => {
      console.log("locked");
      await new Promise(() => setInterval(() => {}, 1000));
    });
  `], { stdout: "pipe", stderr: "pipe" });
  try {
    const reader = child.stdout.getReader();
    const first = await reader.read();
    expect(new TextDecoder().decode(first.value)).toContain("locked");
    reader.releaseLock();
    await expect(withControllerLock(directory, async () => true)).rejects.toThrow("Another controller");
    child.kill("SIGKILL");
    await child.exited;
    expect(await withControllerLock(directory, async () => "recovered")).toBe("recovered");
  } finally {
    child.kill();
    await child.exited;
    rmSync(directory, { recursive: true, force: true });
  }
});
