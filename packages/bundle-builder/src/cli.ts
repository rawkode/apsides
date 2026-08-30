import { resolve } from "node:path";
import { buildBundle } from "./index.js";

// Binary stdout producer: redirect stdout from the shell or container layer
// to publish this archive. The library itself has no publication API.
const result = await buildBundle({ rootDir: resolve("../example-bundle/src") });
process.stdout.write(result.archiveBytes);
