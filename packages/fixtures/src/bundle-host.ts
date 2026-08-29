import { mkdir, writeFile } from "node:fs/promises";
import { rolldown } from "rolldown";

await mkdir("dist", { recursive: true });

await bundleEsm("src/workload.tsx", "dist/workload.js");
await bundleCallable("src/host-react.ts", "OxideLoopReact", "dist/react.callable.js");
await bundleCallable(
  "src/host-react-layout-effect.ts",
  "OxideLoopReactLayoutEffect",
  "dist/react-layout-effect.callable.js",
);
await bundleCallable(
  "src/host-react-like.ts",
  "OxideLoopReactLike",
  "dist/react-like.callable.js",
);
await bundleCallable(
  "src/host-react-like-empty.ts",
  "OxideLoopReactLikeEmpty",
  "dist/react-like-empty.callable.js",
);
await bundleCallable(
  "src/host-react-like-null.ts",
  "OxideLoopReactLikeNull",
  "dist/react-like-null.callable.js",
);

async function bundleEsm(input: string, outputPath: string): Promise<void> {
  const build = await rolldown({
    input,
    define: { "process.env.NODE_ENV": JSON.stringify("production") },
  });
  const generated = await build.generate({ format: "esm", minify: true });
  await writeFile(outputPath, onlyChunk(generated.output), "utf8");
  await build.close();
}

async function bundleCallable(
  input: string,
  globalName: string,
  outputPath: string,
): Promise<void> {
  const build = await rolldown({
    input,
    define: { "process.env.NODE_ENV": JSON.stringify("production") },
  });
  const generated = await build.generate({
    format: "iife",
    name: globalName,
    minify: true,
  });
  const bundle = onlyChunk(generated.output);
  const callable = `(input) => {\n${bundle}\nreturn ${globalName}.render(input);\n}\n`;
  await writeFile(outputPath, callable, "utf8");
  await build.close();
}

function onlyChunk(output: readonly { type: string; code?: string }[]): string {
  const chunks = output.filter(
    (item): item is { type: "chunk"; code: string } => item.type === "chunk",
  );
  if (chunks.length !== 1) {
    throw new Error(`expected one self-contained chunk, received ${chunks.length}`);
  }
  return chunks[0]!.code;
}
