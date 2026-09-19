import { describe, expect, test } from "bun:test";
import {
  Cloudflare, compile, ConfigMap, Controller, Deployment, PlanetScale, Service,
  useResource, useResourceRef, useFinalizer, observationKey, PendingResource,
  type FinalizerContext,
} from "../src";

describe("TSX authoring", () => {
  test("JSX stays inert until a complete graph is compiled", () => {
    let renders = 0;
    function App() {
      renders++;
      const database = useResourceRef<{ host: string }>();
      const service = useResourceRef<{ address: string }>();
      return <Controller name="web" namespace="demo">
        <PlanetScale.Database name="db" organization="example" ref={database} deletionPolicy="retain" />
        <Deployment name="web" image="nginx:stable" port={80} readyAfter={[database]}>
          <Service name="public" type="LoadBalancer" ref={service} />
        </Deployment>
        <Cloudflare.DNSRecord name="web.example.com" zoneId="zone" type="A" content={service.output("address")} />
      </Controller>;
    }
    const element = <App />;
    expect(renders).toBe(0);
    const graph = compile(element);
    expect(renders).toBe(1);
    expect(graph.name).toBe("web");
    expect(graph.resources).toHaveLength(4);
    const [database, deployment, service, dns] = graph.resources;
    expect(database!.deletionPolicy).toBe("retain");
    expect(deployment!.readyAfter).toEqual([database!.id]);
    expect(service!.namespace).toBe("demo");
    expect(service!.props).toEqual({ type: "LoadBalancer", selector: { "app.kubernetes.io/name": "web" }, port: 80, targetPort: 80 });
    expect(service!.dependsOn).toEqual([]);
    expect(dns!.dependsOn).toEqual([service!.id]);
    expect(dns!.props.content).toMatchObject({ tag: "output", path: ["address"] });
  });

  test("nesting context survives ordinary component expansion and respects overrides", () => {
    function Exposure() { return <Service name="public" namespace="other" port={8080} selector={{ app: "override" }} />; }
    const graph = compile(<Controller name="app" namespace="demo">
      <Deployment name="web" image="nginx" ports={[{ containerPort: 80 }]}><Exposure /></Deployment>
    </Controller>);
    expect(graph.resources[1]).toMatchObject({ namespace: "other", props: { port: 8080, targetPort: 8080, selector: { app: "override" } } });
  });

  test("inherits a single container port and rejects an ambiguous default", () => {
    const one = compile(<Controller name="app"><Deployment name="web" image="nginx" ports={[{ containerPort: 8080 }]}>
      <Service name="public" />
    </Deployment></Controller>);
    expect(one.resources[1]!.props.port).toBe(8080);
    expect(() => compile(<Controller name="app"><Deployment name="web" image="nginx" ports={[{ containerPort: 80 }, { containerPort: 443 }]}>
      <Service name="public" />
    </Deployment></Controller>)).toThrow("unambiguous port");
  });

  test("nested Service defaults its name and standalone Service requires one", () => {
    const graph = compile(<Controller name="app"><Deployment name="web" image="nginx" port={80}>
      <Service />
    </Deployment></Controller>);
    expect(graph.resources[1]).toMatchObject({ name: "web", id: "kubernetes:Service/default/web" });
    expect(() => compile(<Controller name="app"><Service selector={{ app: "web" }} port={80} /></Controller>)).toThrow("requires a name");
  });

  test("identities survive ordering changes and component wrappers", () => {
    function Wrapped() { return <ConfigMap name="a" />; }
    const first = compile(<Controller name="app"><Wrapped /><ConfigMap name="b" /></Controller>);
    const second = compile(<Controller name="app"><ConfigMap name="b" /><Wrapped /></Controller>);
    expect(first.resources.map(n => n.id).sort()).toEqual(second.resources.map(n => n.id).sort());
    expect(first.resources[0]!.id).toBe("kubernetes:ConfigMap/default/a");
  });

  test("rejects duplicate logical and physical identities", () => {
    expect(() => compile(<Controller name="app"><ConfigMap name="a" id="same" /><ConfigMap name="b" id="same" /></Controller>)).toThrow("Duplicate resource id");
    expect(() => compile(<Controller name="app"><ConfigMap name="a" id="one" /><ConfigMap name="a" id="two" /></Controller>)).toThrow("Duplicate physical resource");
  });

  test("rejects missing refs and combined data/readiness cycles", () => {
    const missing = useResourceRef<{ value: string }>();
    expect(() => compile(<Controller name="app"><ConfigMap name="a" data={{ value: missing.output("value") }} /></Controller>)).toThrow("Unresolved resource reference");
    const a = useResourceRef<{ value: string }>();
    const b = useResourceRef<{ value: string }>();
    expect(() => compile(<Controller name="app">
      <ConfigMap name="a" ref={a} data={{ value: b.output("value") }} />
      <ConfigMap name="b" ref={b} readyAfter={[a]} />
    </Controller>)).toThrow("dependency cycle");
  });

  test("rejects self dependencies and refs bound only by an older graph", () => {
    const self = useResourceRef<{ value: string }>();
    expect(() => compile(<Controller name="app"><ConfigMap name="a" ref={self} data={{ value: self.output("value") }} /></Controller>)).toThrow("dependency cycle");
    const ready = useResourceRef();
    expect(() => compile(<Controller name="app"><ConfigMap name="a" ref={ready} readyAfter={[ready]} /></Controller>)).toThrow("dependency cycle");
    const old = useResourceRef<{ value: string }>();
    compile(<Controller name="app"><ConfigMap name="a" ref={old} /></Controller>);
    expect(() => compile(<Controller name="app"><ConfigMap name="a" /><ConfigMap name="b" data={{ value: old.output("value") }} /></Controller>)).toThrow("Unresolved resource reference");
  });

  test("rejects unqualified finalizer names", () => {
    expect(() => compile(<Controller name="app"><ConfigMap name="a" finalizers={{ cleanup: async () => {} }} /></Controller>)).toThrow("qualified key");
  });

  test("outputs refuse accidental JavaScript string coercion", () => {
    const ref = useResourceRef<{ address: string }>();
    expect(() => String(ref.output("address"))).toThrow("cannot be coerced");
  });

  test("required ConfigMaps wait inside the hook and normalize envFrom", () => {
    function App() {
      const config = useResource(ConfigMap, { name: "settings" });
      return <Deployment name="web" image="nginx" envFrom={[config]} />;
    }
    const element = <Controller name="app" namespace="demo"><App /></Controller>;
    const plan = compile(element);
    expect(plan.observations).toEqual([{ kind: "kubernetes:ConfigMap", name: "settings", namespace: "demo", access: ["read"] }]);
    expect(() => compile(element, { snapshots: new Map() })).toThrow(PendingResource);
    const snapshots = new Map([[observationKey(plan.observations[0]!), { data: { COLOR: "blue" } }]]);
    const graph = compile(element, { snapshots });
    expect(graph.resources[0]!.props.envFrom).toEqual([{ configMapRef: { name: "settings" } }]);
    expect(JSON.stringify(graph.resources[0]!.props)).not.toContain("blue");
    expect(Object.values(graph.resources[0]!.props.podAnnotations as object)[0]).toMatch(/^[a-f0-9]{64}$/);
  });

  test("checksums change only with content, retaining source precedence", () => {
    function App() {
      const config = useResource(ConfigMap, { name: "settings" });
      return <Deployment name="web" image="nginx" envFrom={[config]} podAnnotations={{ "example.com/custom": "yes" }} />;
    }
    const element = <Controller name="app" namespace="demo"><App /></Controller>;
    const key = observationKey(compile(element).observations[0]!);
    const annotations = (snapshot: Record<string, unknown>) => compile(element, { snapshots: new Map([[key, snapshot]]) }).resources[0]!.props.podAnnotations;
    const first = annotations({ data: { A: "one", B: "two" }, metadata: { uid: "old", resourceVersion: "1" } });
    expect(first).toEqual(annotations({ data: { B: "two", A: "one" }, metadata: { uid: "new", resourceVersion: "99" } }));
    expect(first).not.toEqual(annotations({ data: { A: "changed", B: "two" } }));
    expect(first).toEqual(annotations({ data: { A: "one", B: "two" }, binaryData: { extra: "YQ==" } }));
    expect(first).toMatchObject({ "example.com/custom": "yes" });
    expect(() => annotations({ data: {}, metadata: { deletionTimestamp: "now" } })).toThrow(PendingResource);
  });

  test("unmanaged observations cannot grant writes or cross namespace envFrom", () => {
    function InvalidAccess() {
      // @ts-expect-error unmanaged observations are read-only
      useResource(ConfigMap, { name: "settings", access: ["delete"] });
      return null;
    }
    expect(() => compile(<Controller name="app"><InvalidAccess /></Controller>)).toThrow("read-only");
    function CrossNamespace() {
      const config = useResource(ConfigMap, { name: "settings", namespace: "other" });
      expect("ensureDeleted" in config).toBe(false);
      expect("patch" in config).toBe(false);
      return <Deployment name="web" image="nginx" envFrom={[config]} />;
    }
    expect(() => compile(<Controller name="app"><CrossNamespace /></Controller>)).toThrow("share the Deployment namespace");
  });

  test("removing envFrom clears generated annotations and keeps explicit annotations", () => {
    const without = compile(<Controller name="app"><Deployment name="web" image="nginx" /></Controller>);
    const empty = compile(<Controller name="app"><Deployment name="web" image="nginx" envFrom={[]} /></Controller>);
    expect(without.resources[0]!.props).toEqual(empty.resources[0]!.props);
    expect(without.resources[0]!.props).toMatchObject({ envFrom: [], podAnnotations: {} });
  });

  test("inline finalizer hooks bind refs after rendering and run only on invocation", async () => {
    let invoked = 0;
    function App() {
      const resource = useResourceRef();
      useFinalizer(resource, "example.com/cleanup", async ({ signal, resource }) => {
        expect(signal.aborted).toBe(false);
        expect(resource.uid).toBe("uid");
        invoked++;
      });
      return <Deployment name="web" image="nginx" ref={resource} />;
    }
    const graph = compile(<Controller name="app"><App /></Controller>);
    expect(invoked).toBe(0);
    const context: FinalizerContext = { signal: new AbortController().signal, resource: { id: "web", name: "web", uid: "uid", outputs: {} } };
    await graph.resources[0]!.finalizers["example.com/cleanup"]!(context);
    expect(invoked).toBe(1);
  });

  test("finalizer hooks reject duplicate keys, missing refs and invalid keys", () => {
    function App({ missing = false, finalizerKey = "example.com/cleanup", duplicate = false }) {
      const resource = useResourceRef();
      useFinalizer(resource, finalizerKey, async () => {});
      if (duplicate) useFinalizer(resource, finalizerKey, async () => {});
      return <Deployment name="web" image="nginx" ref={missing ? undefined : resource} />;
    }
    expect(() => compile(<Controller name="app"><App missing /></Controller>)).toThrow("Unresolved resource reference");
    expect(() => compile(<Controller name="app"><App finalizerKey="cleanup" /></Controller>)).toThrow("qualified key");
    expect(() => compile(<Controller name="app"><App duplicate /></Controller>)).toThrow("Duplicate finalizer");
  });
});
