import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";

import { buildBundle } from "./index.js";

const packageRoot = resolve(fileURLToPath(new URL("../", import.meta.url)));
const parent = await mkdtemp(join(packageRoot, ".bundle-builder-controller-"));

const controller = `
import { Event, defineAction, defineController } from "@oxideloop/kubernetes";

interface Spec { message: string }

const configurationChanged = defineAction<{ message: string }>({
  id: "configuration-changed",
  version: "v1",
  run: ({ source, metadata }) => <Event id="notification" regarding={source} reason="ConfigurationChanged" action="ConfigurationChanged" note={metadata.message} />,
});

export default defineController<Spec>(({ root }) => root.metadata.name);
`;

function archiveEntry(archive: Uint8Array, wanted: string): string {
  if (Buffer.from(archive.subarray(0, 4)).toString("ascii") !== "OLBA") throw new Error("invalid bundle archive");
  const count = archive[5];
  if (count === undefined) throw new Error("truncated bundle archive");
  let offset = 6;
  const headers: Array<{ name: string; length: number }> = [];
  for (let index = 0; index < count; index += 1) {
    const nameLength = archive[offset++];
    if (nameLength === undefined) throw new Error("truncated entry name");
    const name = Buffer.from(archive.subarray(offset, offset + nameLength)).toString("utf8");
    offset += nameLength;
    const length = Buffer.from(archive.subarray(offset, offset + 4)).readUInt32BE();
    offset += 4 + 64;
    headers.push({ name, length });
  }
  for (const header of headers) {
    const bytes = archive.subarray(offset, offset + header.length);
    if (header.name === wanted) return Buffer.from(bytes).toString("utf8");
    offset += header.length;
  }
  throw new Error(`missing archive entry: ${wanted}`);
}

async function fixture(name: string, source: string, legacy = false): Promise<string> {
  const root = join(parent, name);
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "controller.tsx"), source, { flag: "wx" });
  if (legacy) await writeFile(join(root, "action.ts"), "export const action = () => undefined;\n", { flag: "wx" });
  return root;
}

async function mustReject(root: string, reason: string): Promise<void> {
  const result = await Promise.allSettled([buildBundle({ rootDir: root })]);
  if (result[0]?.status !== "rejected") throw new Error(`${reason}: build unexpectedly succeeded`);
}

try {
  const validRoot = await fixture("valid", controller);
  const [first, second] = await Promise.all([buildBundle({ rootDir: validRoot }), buildBundle({ rootDir: validRoot })]);
  if (!first.archiveBytes.equals(second.archiveBytes)) throw new Error("controller archive bytes must be deterministic");
  if (first.manifest.actionModules[0]?.identity !== "configuration-changed") {
    throw new Error("action module identity must come from the direct defineAction id");
  }

  const actionCallable = archiveEntry(first.archiveBytes, "action.callable.js");
  const invoke = Function(`return (${actionCallable});`)() as (input: unknown) => unknown;
  const validContext = {
    abiVersion: "renderer.oxideloop.dev/v0",
    bundleDigest: "a".repeat(64),
    automationIdentity: "configuration-changed",
    automationVersion: "v1",
    actionModuleIdentity: "configuration-changed",
    actionModuleDigest: "b".repeat(64),
    occurrenceId: "occurrence-1",
    sourceTransitionId: "transition-1",
    sourceUid: "uid-1",
    sourceResourceVersion: "7",
    eventMetadata: {
      $oxideloop: {
        kind: "action-context",
        version: "v1",
        source: {
          apiVersion: "v1",
          kind: "ConfigMap",
          namespace: "default",
          name: "demo-input",
          uid: "uid-1",
          resourceVersion: "7",
        },
        metadata: { message: "changed" },
      },
    },
  };
  const actionResult = invoke(validContext) as { actions?: Array<{ logicalId?: string; specification?: { regarding?: { name?: string }; note?: string } }> };
  if (actionResult.actions?.[0]?.logicalId !== "notification/occurrence-1" || actionResult.actions[0]?.specification?.regarding?.name !== "demo-input" || actionResult.actions[0]?.specification?.note !== "changed") {
    throw new Error("generated action bridge did not lower the exact tagged source and metadata");
  }
  for (const invalid of [
    { ...validContext, eventMetadata: { message: "untagged" } },
    { ...validContext, eventMetadata: { $oxideloop: { kind: "action-context", version: "v1", source: {}, metadata: {} } } },
    { ...validContext, eventMetadata: { $oxideloop: { kind: "wrong", version: "v1", source: validContext.eventMetadata.$oxideloop.source, metadata: {} } } },
    { ...validContext, eventMetadata: { $oxideloop: { ...validContext.eventMetadata.$oxideloop, source: { ...validContext.eventMetadata.$oxideloop.source, uid: "retarget" } } } },
  ]) {
    let rejected = false;
    try { invoke(invalid); } catch { rejected = true; }
    if (!rejected) throw new Error("generated action accepted malformed action-context v1 metadata");
  }

  await mustReject(await fixture("alias", controller.replace("Event, defineAction", "Event as Notice, defineAction")), "import alias");
  await mustReject(await fixture("exported-action", controller.replace("const configurationChanged", "export const configurationChanged")), "exported action");
  await mustReject(await fixture("global-capture", controller.replace("note={metadata.message}", "note={process.env.MESSAGE}")), "ambient global capture");
  await mustReject(await fixture("unknown-call", controller.replace("note={metadata.message}", "note={unknown(metadata.message)}")), "unknown action call");
  await mustReject(await fixture("legacy", controller, true), "legacy action.ts entrypoint");
  await mustReject(await fixture("top-level-effect", controller.replace("interface Spec", "const startup = 1;\ninterface Spec")), "top-level evaluation");
  await mustReject(await fixture("fetch", controller.replace(
    "export default defineController<Spec>(({ root }) => root.metadata.name);",
    "export default defineController<Spec>(({ root }) => { fetch(\"https://example.invalid\"); return root.metadata.name; });",
  )), "fetch in render");
  await mustReject(await fixture("timer", controller.replace(
    "export default defineController<Spec>(({ root }) => root.metadata.name);",
    "export default defineController<Spec>(({ root }) => { setTimeout(() => undefined, 1); return root.metadata.name; });",
  )), "timer in render");
  await mustReject(await fixture("assignment", controller.replace(
    "export default defineController<Spec>(({ root }) => root.metadata.name);",
    "export default defineController<Spec>(({ root }) => { root.metadata.name = \"mutated\"; return root.metadata.name; });",
  )), "assignment in render");
  await mustReject(await fixture("constructor", controller.replace(
    "export default defineController<Spec>(({ root }) => root.metadata.name);",
    "export default defineController<Spec>(({ root }) => { const now = new Date(); return root.metadata.name; });",
  )), "constructor in render");

  const capabilityController = controller.replace(
    "Event, defineAction, defineController",
    "ConfigMap, Event, defineAction, defineController, useAutomation, useResource",
  ).replace(
    "export default defineController<Spec>(({ root }) => root.metadata.name);",
    "export default defineController<Spec>(({ root }) => { const settings = useResource(ConfigMap, { name: \"demo-input\" }); useAutomation({ when: settings.changed(), action: configurationChanged, metadata: { message: root.metadata.name } }); return null; });",
  );
  await buildBundle({ rootDir: await fixture("capabilities", capabilityController) });

  // The published golden is compiled and invoked as bytes, not merely parsed:
  // its semantic resources and action output must remain ABI-compatible.
  const goldenSource = await readFile(join(packageRoot, "../fixtures/golden/v1/controller.valid.tsx"), "utf8");
  const goldenExpectation = JSON.parse(await readFile(join(packageRoot, "../fixtures/golden/v1/semantic-lowering.json"), "utf8")) as { expected: { resources: unknown[] } };
  const golden = await buildBundle({ rootDir: await fixture("golden", goldenSource) });
  const renderCallable = archiveEntry(golden.archiveBytes, "render.callable.js");
  const render = Function(`return (${renderCallable});`)() as (input: unknown) => unknown;
  const renderInput = {
    abiVersion: "renderer.oxideloop.dev/v0", bundleDigest: golden.bundleDigest, cacheEpoch: "1", observationToken: "golden-token",
    root: { apiVersion: "demo.oxideloop.dev/v1alpha1", kind: "WebApplication", namespace: "demo", name: "portal", uid: "root-uid", generation: 1, spec: { image: "ghcr.io/example/web:v1", replicas: 2, expose: true } },
    observations: [{ apiVersion: "v1", kind: "ConfigMap", namespace: "demo", name: "demo-input", uid: "input-uid", resourceVersion: "9", value: { data: { message: "from-input" } } }],
  };
  const rendered = render(renderInput) as { resources?: unknown[]; dependencies?: Array<{ uid?: string }>; automations?: Array<{ eventMetadata: unknown; actionModuleIdentity: string; actionModuleDigest: string }> };
  const loweredResources = rendered.resources?.map((resource) => {
    const row = resource as { logicalId: string; object: unknown };
    return { logicalId: row.logicalId, object: row.object };
  });
  if (stableJson(loweredResources) !== stableJson(goldenExpectation.expected.resources) || rendered.dependencies?.[0]?.uid !== "input-uid" || rendered.automations?.length !== 1) {
    throw new Error("compiled canonical controller did not lower to the v1 semantic golden");
  }
  const goldenAction = golden.manifest.actionModules[0];
  if (!goldenAction || !rendered.automations?.[0]) throw new Error("compiled canonical controller omitted its action binding");
  const goldenActionCallable = archiveEntry(golden.archiveBytes, "action.callable.js");
  const invokeGoldenAction = Function(`return (${goldenActionCallable});`)() as (input: unknown) => { actions?: Array<{ logicalId?: string; specification?: { regarding?: { name?: string } } }> };
  const goldenActionOutput = invokeGoldenAction({
    abiVersion: "renderer.oxideloop.dev/v0", bundleDigest: golden.bundleDigest, automationIdentity: "configuration-changed", automationVersion: "v1",
    actionModuleIdentity: goldenAction.identity, actionModuleDigest: goldenAction.sha256, occurrenceId: "occurrence-golden", sourceTransitionId: "transition-golden", sourceUid: "input-uid", sourceResourceVersion: "9",
    eventMetadata: rendered.automations[0].eventMetadata,
  });
  if (goldenActionOutput.actions?.[0]?.logicalId !== "notification/occurrence-golden" || goldenActionOutput.actions[0]?.specification?.regarding?.name !== "demo-input") {
    throw new Error("compiled canonical action did not produce a source-bound ActionOutputV0");
  }
} finally {
  await rm(parent, { recursive: true, force: true });
}

function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  return `{${Object.keys(value as Record<string, unknown>).sort().map((key) => `${JSON.stringify(key)}:${stableJson((value as Record<string, unknown>)[key])}`).join(",")}}`;
}
