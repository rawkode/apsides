import type {
  AutomationDeclaration,
  ResourceDependency,
  ResourceHandle,
  ResourceObject,
  ResourceRef,
} from "./index.js";

const MAX_EVENT_METADATA_CANONICAL_BYTES = 512 * 1024;

export interface HookEnvironment {
  readonly observationToken: string;
  read<T extends ResourceObject>(ref: ResourceRef): ResourceHandle<T>;
}

/** Render-owned hook state; it is never shared between render invocations. */
export interface HookSession {
  readonly environment: HookEnvironment;
  readonly dependencies: readonly ResourceDependency[];
  readonly automations: readonly AutomationDeclaration[];
}

interface MutableHookSession extends HookSession {
  readonly dependencyMap: Map<string, ResourceDependency>;
  readonly automationMap: Map<string, AutomationDeclaration>;
}

export function createHookSession(environment: HookEnvironment): HookSession {
  const session: MutableHookSession = {
    environment,
    dependencyMap: new Map(),
    automationMap: new Map(),
    get dependencies() { return ordered(this.dependencyMap.values(), dependencyKey); },
    get automations() { return ordered(this.automationMap.values(), (declaration) => declaration.identity); },
  };
  return session;
}

/** A synchronous convenience that keeps the session explicit to the callback. */
export function withHookEnvironment<T>(environment: HookEnvironment, render: (session: HookSession) => T): T {
  return render(createHookSession(environment));
}

function mutable(session: HookSession): MutableHookSession {
  const candidate = session as MutableHookSession;
  if (!candidate.dependencyMap || !candidate.automationMap) {
    throw new Error("OxideLoop hooks require a session created by createHookSession()");
  }
  return candidate;
}

function dependencyKey(dependency: ResourceDependency | ResourceRef): string {
  return canonicalJson([dependency.apiVersion, dependency.kind, dependency.namespace ?? "", dependency.name]);
}

export function useResource<T extends ResourceObject>(session: HookSession, ref: ResourceRef): ResourceHandle<T> {
  const handle = session.environment.read<T>(ref);
  const observation = handle.observation ?? {
    uid: handle.value?.uid ?? null,
    resourceVersion: handle.value?.resourceVersion ?? null,
  };
  const dependency: ResourceDependency = {
    type: "object",
    apiVersion: handle.ref.apiVersion,
    kind: handle.ref.kind,
    namespace: handle.ref.namespace ?? "",
    name: handle.ref.name,
    uid: observation.uid,
    resourceVersion: observation.resourceVersion,
    observationToken: session.environment.observationToken,
  };
  const hooks = mutable(session);
  const key = dependencyKey(dependency);
  const existing = hooks.dependencyMap.get(key);
  if (existing && canonicalJson(existing) !== canonicalJson(dependency)) {
    throw new Error(`conflicting dependency observation: ${key}`);
  }
  hooks.dependencyMap.set(key, dependency);
  return handle;
}

export function useEnriched<T>(value: T | undefined, enrich: (value: T) => T): T | undefined {
  return value === undefined ? undefined : enrich(value);
}

/** Declare a durable automation with a render-local identity registry. */
export function useAutomation(session: HookSession, declaration: AutomationDeclaration): AutomationDeclaration {
  const accepted = validateAutomation(declaration);
  const hooks = mutable(session);
  const existing = hooks.automationMap.get(accepted.identity);
  if (existing && canonicalJson(existing) !== canonicalJson(accepted)) {
    throw new Error(`conflicting automation declaration: ${accepted.identity}`);
  }
  hooks.automationMap.set(accepted.identity, accepted);
  return accepted;
}

function validateAutomation(declaration: AutomationDeclaration): AutomationDeclaration {
  if (declaration === null || typeof declaration !== "object") throw new Error("invalid automation declaration");
  const sourceDependency = snapshotSourceDependency(declaration.sourceDependency);
  const eventMetadata = snapshotJson(declaration.eventMetadata);
  const accepted = Object.freeze({
    identity: declaration.identity,
    version: declaration.version,
    sourceDependency,
    eventMetadata: eventMetadata.value as AutomationDeclaration["eventMetadata"],
    bundleDigest: declaration.bundleDigest,
    actionModuleIdentity: declaration.actionModuleIdentity,
    actionModuleDigest: declaration.actionModuleDigest,
    rendererAbiVersion: declaration.rendererAbiVersion,
  });
  if (!accepted.identity || !accepted.version || !accepted.actionModuleIdentity ||
      accepted.rendererAbiVersion !== "renderer.oxideloop.dev/v0" ||
      !isRawDigest(accepted.bundleDigest) || !isRawDigest(accepted.actionModuleDigest)) {
    throw new Error("invalid automation declaration");
  }
  if (utf8Bytes(eventMetadata.canonical).length > MAX_EVENT_METADATA_CANONICAL_BYTES) {
    throw new Error("automation eventMetadata exceeds the canonical byte limit");
  }
  const source = accepted.sourceDependency;
  if (source.type !== "object" || !source.apiVersion || !source.kind || !source.namespace ||
      !source.name || !source.uid || !source.resourceVersion || !source.observationToken) {
    throw new Error("automation sourceDependency must be an observed object dependency");
  }
  return accepted;
}

function snapshotSourceDependency(value: unknown): AutomationDeclaration["sourceDependency"] {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("automation sourceDependency must be an object dependency");
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) throw new Error("automation sourceDependency must be a plain object");
  const expected = ["type", "apiVersion", "kind", "namespace", "name", "uid", "resourceVersion", "observationToken"] as const;
  const descriptors = Object.getOwnPropertyDescriptors(value);
  const ownKeys = Reflect.ownKeys(value);
  if (ownKeys.some(key => typeof key === "symbol") || ownKeys.length !== expected.length || expected.some(key => !ownKeys.includes(key))) {
    throw new Error("automation sourceDependency has unknown or missing fields");
  }
  const fields = Object.fromEntries(expected.map(key => {
    const descriptor = descriptors[key];
    if (!descriptor || !descriptor.enumerable || !("value" in descriptor) || typeof descriptor.value !== "string" || descriptor.value.length === 0) {
      throw new Error("automation sourceDependency fields must be nonempty strings");
    }
    return [key, descriptor.value];
  })) as Record<(typeof expected)[number], string>;
  if (fields.type !== "object") throw new Error("automation sourceDependency must be an object dependency");
  // Apply the same string/surrogate limits as Rust before projecting the
  // exact source shape retained by the hook session.
  for (const key of expected) {
    if (hasUnpairedSurrogate(fields[key]) || utf8Bytes(fields[key]).length > 256 * 1024) {
      throw new Error(`automation sourceDependency.${key} is not valid JSON text`);
    }
  }
  return Object.freeze({
    type: "object" as const,
    apiVersion: fields.apiVersion,
    kind: fields.kind,
    namespace: fields.namespace,
    name: fields.name,
    uid: fields.uid,
    resourceVersion: fields.resourceVersion,
    observationToken: fields.observationToken,
  });
}

interface JsonSnapshot {
  readonly value: unknown;
  readonly canonical: string;
}

/** Snapshot the strict JSON subset accepted by Rust before retaining it. */
function snapshotJson(value: unknown, depth = 0, ancestors = new Set<object>()): JsonSnapshot {
  if (depth > 64) throw new Error("automation eventMetadata exceeds JSON depth limit");
  if (value === null) return { value: null, canonical: "null" };
  if (typeof value === "string") {
    if (hasUnpairedSurrogate(value)) throw new Error("automation eventMetadata contains an invalid string");
    if (utf8Bytes(value).length > 256 * 1024) throw new Error("automation eventMetadata contains an oversized string");
    return { value, canonical: JSON.stringify(value) };
  }
  if (typeof value === "boolean") return { value, canonical: value ? "true" : "false" };
  if (typeof value === "number") {
    if (!Number.isFinite(value) || Object.is(value, -0) || (Number.isInteger(value) && !Number.isSafeInteger(value))) {
      throw new Error("invalid JSON number");
    }
    return { value, canonical: JSON.stringify(value) };
  }
  if (typeof value !== "object") throw new Error("invalid JSON value");
  if (ancestors.has(value)) throw new Error("cyclic JSON value");
  ancestors.add(value);
  try {
    if (Array.isArray(value)) return snapshotArray(value, depth, ancestors);
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null) throw new Error("JSON metadata must contain plain objects");
    const descriptors = Object.getOwnPropertyDescriptors(value);
    if (Reflect.ownKeys(value).some(key => typeof key === "symbol")) throw new Error("JSON metadata cannot contain symbols");
    const entries: Array<[string, JsonSnapshot]> = [];
    for (const [key, descriptor] of Object.entries(descriptors)) {
      if (!descriptor.enumerable || !("value" in descriptor)) throw new Error("JSON metadata cannot contain hidden properties or getters");
      if (hasUnpairedSurrogate(key)) throw new Error("automation eventMetadata contains an invalid key");
      if (utf8Bytes(key).length > 256 * 1024) throw new Error("automation eventMetadata contains an oversized key");
      entries.push([key, snapshotJson(descriptor.value, depth + 1, ancestors)]);
    }
    entries.sort((left, right) => compareUtf16(left[0], right[0]));
    const object = Object.create(null) as Record<string, unknown>;
    for (const [key, entry] of entries) Object.defineProperty(object, key, { value: entry.value, enumerable: true, writable: false, configurable: false });
    Object.freeze(object);
    return { value: object, canonical: `{${entries.map(([key, entry]) => `${JSON.stringify(key)}:${entry.canonical}`).join(",")}}` };
  } finally {
    ancestors.delete(value);
  }
}

function snapshotArray(value: unknown[], depth: number, ancestors: Set<object>): JsonSnapshot {
  if (value.length > MAX_EVENT_METADATA_CANONICAL_BYTES) throw new Error("automation eventMetadata array is too large");
  const descriptors = Object.getOwnPropertyDescriptors(value);
  if (Reflect.ownKeys(value).some(key => typeof key === "symbol")) throw new Error("JSON metadata cannot contain symbols");
  const entries: JsonSnapshot[] = [];
  for (let index = 0; index < value.length; index += 1) {
    const descriptor = descriptors[String(index)];
    if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) throw new Error("JSON metadata arrays must be dense and data-only");
    entries.push(snapshotJson(descriptor.value, depth + 1, ancestors));
  }
  for (const [key, descriptor] of Object.entries(descriptors)) {
    if (key !== "length" && (!/^\d+$/.test(key) || Number(key) >= value.length || !descriptor.enumerable || !("value" in descriptor))) {
      throw new Error("JSON metadata arrays cannot contain extra properties");
    }
  }
  const array = entries.map(entry => entry.value);
  Object.freeze(array);
  return { value: array, canonical: `[${entries.map(entry => entry.canonical).join(",")}]` };
}

function compareUtf16(left: string, right: string): number {
  const leftUnits = [...left].flatMap(character => {
    const code = character.codePointAt(0) ?? 0;
    return code > 0xffff ? [0xd800 + ((code - 0x10000) >> 10), 0xdc00 + ((code - 0x10000) & 0x3ff)] : [code];
  });
  const rightUnits = [...right].flatMap(character => {
    const code = character.codePointAt(0) ?? 0;
    return code > 0xffff ? [0xd800 + ((code - 0x10000) >> 10), 0xdc00 + ((code - 0x10000) & 0x3ff)] : [code];
  });
  for (let index = 0; index < Math.min(leftUnits.length, rightUnits.length); index += 1) {
    const difference = (leftUnits[index] ?? 0) - (rightUnits[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return leftUnits.length - rightUnits.length;
}

function hasUnpairedSurrogate(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return true;
      index += 1;
    } else if (code >= 0xdc00 && code <= 0xdfff) return true;
  }
  return false;
}

function isRawDigest(value: string): boolean { return /^[0-9a-f]{64}$/.test(value); }

function ordered<T>(values: Iterable<T>, key: (value: T) => string): readonly T[] {
  return [...values].sort((left, right) => compareUtf8(key(left), key(right)));
}

function compareUtf8(left: string, right: string): number {
  const leftBytes = utf8Bytes(left);
  const rightBytes = utf8Bytes(right);
  for (let index = 0; index < Math.min(leftBytes.length, rightBytes.length); index += 1) {
    const difference = (leftBytes[index] ?? 0) - (rightBytes[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return leftBytes.length - rightBytes.length;
}

function utf8Bytes(value: string): number[] {
  const bytes: number[] = [];
  for (let index = 0; index < value.length; index += 1) {
    let codePoint = value.charCodeAt(index);
    if (codePoint >= 0xd800 && codePoint <= 0xdbff && index + 1 < value.length) {
      const low = value.charCodeAt(index + 1);
      if (low >= 0xdc00 && low <= 0xdfff) {
        codePoint = 0x10000 + ((codePoint - 0xd800) << 10) + (low - 0xdc00);
        index += 1;
      } else codePoint = 0xfffd;
    } else if (codePoint >= 0xdc00 && codePoint <= 0xdfff) codePoint = 0xfffd;
    if (codePoint <= 0x7f) bytes.push(codePoint);
    else if (codePoint <= 0x7ff) bytes.push(0xc0 | (codePoint >> 6), 0x80 | (codePoint & 0x3f));
    else if (codePoint <= 0xffff) bytes.push(0xe0 | (codePoint >> 12), 0x80 | ((codePoint >> 6) & 0x3f), 0x80 | (codePoint & 0x3f));
    else bytes.push(0xf0 | (codePoint >> 18), 0x80 | ((codePoint >> 12) & 0x3f), 0x80 | ((codePoint >> 6) & 0x3f), 0x80 | (codePoint & 0x3f));
  }
  return bytes;
}

/** Canonical JSON used only for deterministic local identity/equality checks. */
function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const object = value as Record<string, unknown>;
  return `{${Object.keys(object).sort(compareUtf8).map((key) => `${JSON.stringify(key)}:${canonicalJson(object[key])}`).join(",")}}`;
}

export type ReadHook = <T extends ResourceObject = ResourceObject>(session: HookSession, ref: ResourceRef) => ResourceHandle<T>;
