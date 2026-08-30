import type {
  AutomationDeclaration,
  ResourceDependency,
  ResourceHandle,
  ResourceObject,
  ResourceRef,
} from "./index.js";

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
  validateAutomation(declaration);
  const hooks = mutable(session);
  const existing = hooks.automationMap.get(declaration.identity);
  if (existing && canonicalJson(existing) !== canonicalJson(declaration)) {
    throw new Error(`conflicting automation declaration: ${declaration.identity}`);
  }
  hooks.automationMap.set(declaration.identity, declaration);
  return declaration;
}

function validateAutomation(declaration: AutomationDeclaration): void {
  if (!declaration.identity || !declaration.version || !declaration.actionModuleIdentity ||
      declaration.rendererAbiVersion !== "renderer.oxideloop.dev/v0" ||
      !isRawDigest(declaration.bundleDigest) || !isRawDigest(declaration.actionModuleDigest)) {
    throw new Error("invalid automation declaration");
  }
  const source = declaration.sourceDependency;
  if (source.type !== "object" || !source.apiVersion || !source.kind || !source.namespace ||
      !source.name || !source.uid || !source.resourceVersion || !source.observationToken) {
    throw new Error("automation sourceDependency must be an observed object dependency");
  }
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
