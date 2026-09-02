import { readFileSync } from "node:fs";

type Json = null | boolean | number | string | Json[] | { readonly [key: string]: Json };

const fixture = (name: string): string => readFileSync(new URL(`../golden/v1/${name}`, import.meta.url), "utf8");
const controller = fixture("controller.valid.tsx");
const lowering = JSON.parse(fixture("semantic-lowering.json")) as {
  readonly contract: string;
  readonly expected: { readonly resources: readonly { readonly logicalId: string; readonly object: { readonly apiVersion: string; readonly kind: string; readonly metadata: { readonly namespace: string; readonly name: string } } }[] };
};
const vectors = JSON.parse(fixture("normalization-vectors.json")) as {
  readonly policy: string;
  readonly accepted: readonly { readonly source: string; readonly canonical: string }[];
  readonly rejected: readonly string[];
};

if (lowering.contract !== "oxideloop-controller-tsx/v1" || vectors.policy !== "oxideloop-json/v1") {
  throw new Error("v1 fixture versions changed without an explicit contract revision");
}

for (const required of [
  'from "@oxideloop/kubernetes";',
  "interface WebApplicationSpec {",
  "const configurationChanged = defineAction<{ message: string }>({",
  'id: "configuration-changed",',
  'version: "v1",',
  'export default defineController<WebApplicationSpec>(({ root }) => {',
  'useResource(ConfigMap, { name: "demo-input" })',
  'settings.current?.data?.message ?? "Hello from OxideLoop"',
  "const Defaults = useEnrichment({",
  "when: settings.changed(),",
  "action: configurationChanged,",
  '<ConfigMap id="config" name={configName}>',
  'selector={labels}',
  '<Container name="web" image={root.spec.image}>',
  'configMap={{ name: configName, key: "message" }}',
  'requests={{ cpu: "10m", memory: "32Mi" }}',
  'limits={{ memory: "64Mi" }}',
  '<Service id="service" name={`${name}-service`} selector={labels}>',
] as const) {
  if (!controller.includes(required)) throw new Error(`canonical controller fixture is missing ${required}`);
}
for (const forbidden of [
  "@oxideloop/sdk",
  "apiVersion:",
  ".or(",
  ".equals(",
  "useEnrichment(ConfigMap",
  "export const configurationChanged",
] as const) {
  if (controller.includes(forbidden)) throw new Error(`canonical controller fixture contains rejected surface ${forbidden}`);
}
if ((controller.match(/\bdefineAction(?:<[^>]+>)?\(/g)?.length ?? 0) !== 1 ||
    (controller.match(/\bdefineController(?:<[^>]+>)?\(/g)?.length ?? 0) !== 1) {
  throw new Error("canonical controller fixture lost its single action/controller definition");
}

const keys = lowering.expected.resources.map((resource) => [
  resource.object.apiVersion,
  resource.object.kind,
  resource.object.metadata.namespace,
  resource.object.metadata.name,
  resource.logicalId,
] as const);
const ordered = [...keys].sort(compareTuple);
if (JSON.stringify(keys) !== JSON.stringify(ordered)) {
  throw new Error("semantic lowering fixture is not in v1 managed-resource order");
}
if (new Set(lowering.expected.resources.map((resource) => resource.logicalId)).size !== lowering.expected.resources.length) {
  throw new Error("semantic lowering fixture has duplicate logical ids");
}

for (const vector of vectors.accepted) {
  if (canonicalJson(JSON.parse(vector.source) as Json) !== vector.canonical) {
    throw new Error(`normalization vector changed: ${vector.source}`);
  }
}
for (const expected of ["accessor", "cycle", "extra-array-property", "negative-zero", "non-finite", "oversized-string", "sparse-array", "symbol-key", "unsafe-integer", "unpaired-surrogate"] as const) {
  if (!vectors.rejected.includes(expected)) throw new Error(`normalization rejection is missing: ${expected}`);
}

expectRejected(() => canonicalJson(-0), "negative zero");
expectRejected(() => canonicalJson(Number.POSITIVE_INFINITY), "non-finite number");
expectRejected(() => canonicalJson(9_007_199_254_740_992), "unsafe integer");
expectRejected(() => canonicalJson("\ud800"), "unpaired surrogate");
expectRejected(() => canonicalJson("x".repeat(256 * 1024 + 1)), "oversized string");
const sparse: Json[] = [];
sparse.length = 1;
expectRejected(() => canonicalJson(sparse), "sparse array");
const decorated = ["value"] as Json[] & { extra?: string };
decorated.extra = "no";
expectRejected(() => canonicalJson(decorated), "extra array property");
const cyclic: { self?: unknown } = {};
cyclic.self = cyclic;
expectRejected(() => canonicalJson(cyclic), "cycle");
const accessor = {} as { readonly value: string };
Object.defineProperty(accessor, "value", { enumerable: true, get: () => "no" });
expectRejected(() => canonicalJson(accessor), "accessor");

console.log("v1 contract fixtures: ok");

function compareTuple(left: readonly string[], right: readonly string[]): number {
  for (let index = 0; index < left.length; index += 1) {
    if (left[index]! < right[index]!) return -1;
    if (left[index]! > right[index]!) return 1;
  }
  return 0;
}

function canonicalJson(value: unknown, depth = 0, ancestors = new Set<object>()): string {
  if (depth > 64) throw new Error("depth");
  if (value === null || typeof value === "boolean") return JSON.stringify(value);
  if (typeof value === "string") {
    if (hasUnpairedSurrogate(value) || new TextEncoder().encode(value).length > 256 * 1024) throw new Error("string");
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value) || Object.is(value, -0) || (Number.isInteger(value) && !Number.isSafeInteger(value))) throw new Error("number");
    return JSON.stringify(value);
  }
  if (typeof value !== "object") throw new Error("non-json");
  if (ancestors.has(value)) throw new Error("cycle");
  ancestors.add(value);
  try {
    if (Array.isArray(value)) {
      const descriptors = Object.getOwnPropertyDescriptors(value);
      if (Reflect.ownKeys(value).some((key) => typeof key === "symbol")) throw new Error("symbol");
      const items = Array.from({ length: value.length }, (_unused, index) => {
        const descriptor = descriptors[String(index)];
        if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new Error("sparse");
        return canonicalJson(descriptor.value, depth + 1, ancestors);
      });
      if (Object.entries(descriptors).some(([key, descriptor]) => key !== "length" &&
        (!/^\d+$/.test(key) || Number(key) >= value.length || !("value" in descriptor) || !descriptor.enumerable))) {
        throw new Error("array property");
      }
      return `[${items.join(",")}]`;
    }
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null) throw new Error("prototype");
    if (Reflect.ownKeys(value).some((key) => typeof key === "symbol")) throw new Error("symbol");
    const descriptors = Object.getOwnPropertyDescriptors(value);
    return `{${Object.keys(descriptors).sort(compareUtf16).map((key) => {
      const descriptor = descriptors[key];
      if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new Error("accessor");
      if (hasUnpairedSurrogate(key) || new TextEncoder().encode(key).length > 256 * 1024) throw new Error("key");
      return `${JSON.stringify(key)}:${canonicalJson(descriptor.value, depth + 1, ancestors)}`;
    }).join(",")}}`;
  } finally {
    ancestors.delete(value);
  }
}

function compareUtf16(left: string, right: string): number {
  const length = Math.min(left.length, right.length);
  for (let index = 0; index < length; index += 1) {
    const result = left.charCodeAt(index) - right.charCodeAt(index);
    if (result !== 0) return result;
  }
  return left.length - right.length;
}

function hasUnpairedSurrogate(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return true;
      index += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

function expectRejected(operation: () => unknown, label: string): void {
  try {
    operation();
  } catch {
    return;
  }
  throw new Error(`v1 normalizer accepted ${label}`);
}
