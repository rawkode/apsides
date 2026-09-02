import {
  ConfigMapDescriptor,
  createCompilerActionBindings,
  defineAction,
  defineController,
  useAutomation,
  useEnrichment,
  useResource,
  withHookEnvironment,
} from "./index.js";
import type { HookEnvironment } from "./hooks.js";
import type {
  CompilerActionBindings,
  ResourceHandle,
  ResourceObject,
  ResourceRef,
} from "./index.js";

interface WebApplicationSpec {
  readonly image: string;
}

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function expectThrow(run: () => unknown, pattern: RegExp): void {
  try {
    run();
  } catch (error) {
    assert(error instanceof Error && pattern.test(error.message), `expected ${pattern}, received ${String(error)}`);
    return;
  }
  throw new Error(`expected ${pattern} to throw`);
}

interface Observation {
  readonly uid: string | null;
  readonly resourceVersion: string | null;
}

function environment(
  token: string,
  options: {
    readonly observation?: Observation;
    readonly compilerActionBindings?: CompilerActionBindings;
  } = {},
): HookEnvironment {
  const observation = options.observation ?? { uid: `uid-settings`, resourceVersion: `rv-${token}` };
  return {
    observationToken: token,
    ...(options.compilerActionBindings === undefined ? {} : { compilerActionBindings: options.compilerActionBindings }),
    read<T extends ResourceObject>(ref: ResourceRef) {
      const value = {
        ...ref,
        uid: observation.uid,
        resourceVersion: observation.resourceVersion,
        data: { message: "hello" },
      } as unknown as T;
      return {
        ref: { ...ref, namespace: ref.namespace ?? "default" },
        value,
        present: true,
        observation,
      } as ResourceHandle<T>;
    },
  };
}

expectThrow(() => useResource(ConfigMapDescriptor, { name: "settings" }), /only run during/);
expectThrow(() => useEnrichment({ labels: { app: "web" } }), /only run during/);

const rendered = withHookEnvironment(environment("turn-one"), (session) => {
  const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
  assert(configuration.current?.data?.message === "hello", "descriptor resource reads its typed current value");
  assert(configuration.changed() === configuration.changed(), "changed capability is stable for one read");
  assert(Reflect.ownKeys(configuration.changed()).length === 0, "changed capability has no public fields");

  const legacy = useResource(session, { apiVersion: "v1", kind: "Secret", name: "legacy" });
  assert(legacy.present, "legacy explicit-session useResource remains available");
  assert(session.dependencies.length === 2, "both hook forms retain dependencies in the active session");
  return configuration.changed();
});
assert(Reflect.ownKeys(rendered).length === 0, "a capability cannot be projected as source data");

expectThrow(() => withHookEnvironment(environment("turn-two"), () => {
  withHookEnvironment(environment("nested"), () => undefined);
}), /not reentrant/);
expectThrow(() => withHookEnvironment(environment("turn-async"), () => Promise.resolve(undefined)), /must be synchronous/);
withHookEnvironment(environment("turn-isolated"), (session) => {
  assert(session.dependencies.length === 0 && session.automations.length === 0, "each render starts with isolated hook state");
});
withHookEnvironment(environment("turn-three"), () => undefined);

const Controller = defineController<WebApplicationSpec>(({ root }) => {
  const configuration = useResource(ConfigMapDescriptor, { name: root.metadata.name });
  return `${root.spec.image}:${configuration.current?.data?.message ?? "missing"}`;
});
const controllerResult = withHookEnvironment(environment("turn-four"), () => Controller({
  root: {
    metadata: { name: "settings", namespace: "default" },
    spec: { image: "example/web" },
  },
}));
assert(controllerResult === "example/web:hello", "defineController preserves the typed root projection");

const ConfigurationChanged = defineAction<{ message: string; $oxideloop: string }>({
  id: "configuration-changed",
  version: "v1",
  run: ({ source, metadata }) => ({
    abiVersion: "renderer.oxideloop.dev/v0",
    actions: [{ logicalId: source.name, kind: metadata.message, specification: {} }],
    diagnostics: [],
  }),
});
assert(Object.isFrozen(ConfigurationChanged), "object-form actions are branded frozen descriptors");
assert(!("bundleDigest" in ConfigurationChanged), "authored actions cannot carry compiler bindings");
expectThrow(() => defineAction({
  id: "bad-action",
  version: "v1",
  run: () => ({ abiVersion: "renderer.oxideloop.dev/v0", actions: [], diagnostics: [] }),
  bundleDigest: "a".repeat(64),
} as never), /requires id, version, and run/);

const bindings = createCompilerActionBindings([{
  action: ConfigurationChanged,
  bundleDigest: "a".repeat(64),
  actionModuleIdentity: "bundle/actions/configuration-changed",
  actionModuleDigest: "b".repeat(64),
}]);

const declaration = withHookEnvironment(environment("turn-automation", { compilerActionBindings: bindings }), (session) => {
  const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
  const lowered = useAutomation({
    when: configuration.changed(),
    action: ConfigurationChanged,
    metadata: { message: "reload", $oxideloop: "authored-metadata" },
  });
  assert(lowered !== undefined, "an exactly observed resource declares automation");
  assert(session.automations.length === 1, "object-form automation enters the session registry");
  return lowered;
});
assert(declaration.identity === "configuration-changed" && declaration.version === "v1", "action id and version lower to ABI identity");
assert(declaration.bundleDigest === "a".repeat(64), "compiler binding supplies the bundle digest");
assert(declaration.actionModuleIdentity === "bundle/actions/configuration-changed", "compiler binding supplies action identity");
assert(declaration.actionModuleDigest === "b".repeat(64), "compiler binding supplies action digest");
const envelope = declaration.eventMetadata as {
  $oxideloop: {
    kind: string;
    version: string;
    source: Record<string, string>;
    metadata: { message: string; $oxideloop: string };
  };
};
assert(Object.keys(envelope).join(",") === "$oxideloop", "event metadata has no fields outside the action envelope");
assert(Object.keys(envelope.$oxideloop).join(",") === "kind,metadata,source,version", "action envelope has exactly the approved fields");
assert(envelope.$oxideloop.kind === "action-context" && envelope.$oxideloop.version === "v1", "event metadata has the approved action envelope");
assert(Object.keys(envelope.$oxideloop.source).join(",") === "apiVersion,kind,name,namespace,resourceVersion,uid", "event metadata source has exactly the approved fields");
assert(
  envelope.$oxideloop.source.apiVersion === "v1" &&
  envelope.$oxideloop.source.kind === "ConfigMap" &&
  envelope.$oxideloop.source.namespace === "default" &&
  envelope.$oxideloop.source.name === "settings" &&
  envelope.$oxideloop.source.uid === "uid-settings" &&
  envelope.$oxideloop.source.resourceVersion === "rv-turn-automation",
  "event metadata source has exactly the approved pins",
);
assert(envelope.$oxideloop.metadata.$oxideloop === "authored-metadata", "authored $oxideloop metadata remains nested user data");

const absent = withHookEnvironment(environment("turn-absent", {
  compilerActionBindings: bindings,
  observation: { uid: null, resourceVersion: null },
}), (session) => {
  const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
  const lowered = useAutomation({
    when: configuration.changed(),
    action: ConfigurationChanged,
    metadata: { message: "ignored", $oxideloop: "authored-metadata" },
  });
  assert(session.automations.length === 0, "absence does not retain an automation declaration");
  return lowered;
});
assert(absent === undefined, "absence of uid or resourceVersion emits no automation declaration");

for (const observation of [
  { uid: null, resourceVersion: "rv-present" },
  { uid: "uid-present", resourceVersion: null },
]) {
  const partial = withHookEnvironment(environment("turn-partial-absence", {
    compilerActionBindings: bindings,
    observation,
  }), (session) => {
    const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
    const lowered = useAutomation({
      when: configuration.changed(),
      action: ConfigurationChanged,
      metadata: { message: "ignored", $oxideloop: "authored-metadata" },
    });
    assert(session.automations.length === 0, "partial absence does not retain an automation declaration");
    return lowered;
  });
  assert(partial === undefined, "a missing uid or resourceVersion emits no automation declaration");
}

expectThrow(() => useAutomation({
  when: rendered,
  action: ConfigurationChanged,
  metadata: { message: "outside", $oxideloop: "authored-metadata" },
}), /only run during/);
expectThrow(() => withHookEnvironment(environment("turn-reuse", { compilerActionBindings: bindings }), () => useAutomation({
  when: rendered,
  action: ConfigurationChanged,
  metadata: { message: "reuse", $oxideloop: "authored-metadata" },
})), /cannot cross render/);
withHookEnvironment(environment("turn-malformed", { compilerActionBindings: bindings }), () => {
  const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
  expectThrow(() => useAutomation({
    when: {} as never,
    action: ConfigurationChanged,
    metadata: { message: "malformed", $oxideloop: "authored-metadata" },
  }), /changed\(\) trigger/);
  expectThrow(() => useAutomation({
    when: configuration.changed(),
    action: ConfigurationChanged,
    metadata: { message: Number.NaN, $oxideloop: "authored-metadata" } as never,
  }), /invalid JSON number/);
});
withHookEnvironment(environment("turn-unbound"), () => {
  const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
  expectThrow(() => useAutomation({
    when: configuration.changed(),
    action: ConfigurationChanged,
    metadata: { message: "unbound", $oxideloop: "authored-metadata" },
  }), /not bound/);
});
withHookEnvironment(environment("turn-forged-binding", { compilerActionBindings: {} as never }), () => {
  const configuration = useResource(ConfigMapDescriptor, { name: "settings" });
  expectThrow(() => useAutomation({
    when: configuration.changed(),
    action: ConfigurationChanged,
    metadata: { message: "forged", $oxideloop: "authored-metadata" },
  }), /unrecognized compiler action binding registry/);
});

const mutableConfig = {
  labels: { app: "web" },
  ports: [8080, 8443],
};
const Enrichment = withHookEnvironment(environment("turn-five"), () => useEnrichment(mutableConfig));
mutableConfig.labels.app = "mutated";
mutableConfig.ports.push(9000);
assert(Enrichment.config.labels.app === "web", "enrichment config is copied before caller mutation");
assert(Enrichment.config.ports.length === 2, "enrichment arrays are copied before caller mutation");
assert(Object.isFrozen(Enrichment.config), "enrichment config root is frozen");
assert(Object.isFrozen(Enrichment.config.labels), "enrichment nested objects are frozen");
assert(Object.isFrozen(Enrichment.config.ports), "enrichment nested arrays are frozen");
assert(Enrichment({ children: "child" }) === "child", "enrichment wrapper is a transparent component");

withHookEnvironment(environment("turn-six"), () => {
  expectThrow(() => useEnrichment({ invalid: Number.NaN }), /invalid JSON number/);
  expectThrow(() => useEnrichment({ invalid: new Date() as unknown as string }), /plain objects/);
  expectThrow(() => useEnrichment([, "sparse"] as unknown as string[]), /dense/);
});
