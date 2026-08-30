import { resolve } from "node:path";
import { buildBundle } from "./index.js";
const outDir = resolve(process.argv[2] ?? "dist");
const result = await buildBundle({ rootDir: resolve("../example-bundle/src"), outDir });
console.log(JSON.stringify(result, null, 2));
