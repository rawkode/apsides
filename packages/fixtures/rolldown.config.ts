import { defineConfig } from "rolldown";
export default defineConfig({ input: "src/workload.tsx", output: { file: "dist/workload.js", format: "esm" } });
