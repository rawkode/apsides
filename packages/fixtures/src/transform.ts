import { transform } from "oxc-transform";
import { readFileSync } from "node:fs";
const source = readFileSync(new URL("./workload.tsx", import.meta.url), "utf8");
const result = transform("workload.tsx", source, { lang: "tsx", jsx: { runtime: "automatic", importSource: "@oxideloop/react-like-runtime" } });
if (!result.code.includes("jsx")) throw new Error("Oxc did not emit automatic JSX runtime calls");
console.log("Oxc transform: ok");
