import {
  ConfigMap,
  Deployment,
  Secret,
  Service,
  defineDomain,
  generateFixtureBundle,
  materializeFixtureIR,
  watch,
} from "@reakt/kubernetes";

const sourceFile = "program.ts";
const at = (line: number) => ({ file: sourceFile, line, column: 3 });

export default function program(buildInputs: { readonly sourceDigest: string }) {
  return buildProgram(buildInputs.sourceDigest);
}

/** The build wrapper supplies the digest; this author module performs no filesystem or network I/O. */
export function buildProgram(sourceDigest: string) {
  const domain = defineDomain({
    id: "team-api-production",
    namespace: "production",
    controlNamespace: "reakt-system",
    clusterBinding: "local-fixture",
    policyRef: "platform/team-api",
    enforcement: "api-bound",
    budgets: {
      maxResources: 16,
      maxInputs: 16,
      maxReactions: 16,
      maxObjectBytes: 262_144,
      maxQueueDepth: 128,
      maxConcurrentRequests: 4,
    },
    provenance: { inputs: [{ path: sourceFile, sha256: sourceDigest }] },
  });

  const config = domain.manage(new ConfigMap({
    id: "config",
    metadata: { name: "api-config", namespace: "production" },
    data: { LOG_LEVEL: "info" },
  }), { deletionPolicy: "orphan", source: at(41) });

  const settings = domain.observe(ConfigMap, {
    id: "external-settings",
    name: "external-settings",
    namespace: "production",
    projection: ["data"],
    missing: "block-dependent-mutations",
    source: at(47),
  });

  const credentials = domain.observe(Secret, {
    id: "database-credentials",
    name: "database-credentials",
    namespace: "production",
    projection: ["data"],
    missing: "block-dependent-mutations",
    source: at(56),
  });

  const api = domain.manage(new Deployment({
    id: "api",
    metadata: { name: "api", namespace: "production" },
    spec: {
      replicas: 3,
      selector: { matchLabels: { app: "api" } },
      template: {
        metadata: { labels: { app: "api" } },
        spec: {
          serviceAccountName: "api-workload",
          containers: [{
            name: "api",
            image: "registry.example.com/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ports: [{ containerPort: 8080 }],
            envFrom: [
              { configMapRef: { name: "api-config" } },
              { configMapRef: { name: "external-settings" } },
              { secretRef: { name: "database-credentials" } },
            ],
          }],
        },
      },
    },
  }), { deletionPolicy: "orphan", source: at(65) });

  domain.manage(new Service({
    id: "service",
    metadata: { name: "api", namespace: "production" },
    spec: { selector: { app: "api" }, ports: [{ port: 80, targetPort: 8080 }] },
  }), { deletionPolicy: "orphan", source: at(90) });

  watch(config).project("data").rollout(api, { id: "config-rollout", source: at(94) });
  watch(settings).project("data").rollout(api, { id: "settings-rollout", source: at(95) });
  watch(credentials).project("data").rollout(api, { id: "credentials-rollout", source: at(96) });

  const productionIR = domain.toIR();
  const fixtures = generateFixtureBundle(productionIR, { workloadImage: "reakt-fixture:local" });
  return { productionIR, fixtures, fixtureIR: materializeFixtureIR(productionIR, fixtures) } as const;
}
